use std::sync::{Arc, Mutex, Weak};

use anyhow::Result;
use gpu_alloc::{GpuAllocator, MemoryBlock};
use gpu_alloc_vulkanalia::AsMemoryDevice;
use shared::util::WithDefer;
use smallvec::SmallVec;
use vulkanalia::prelude::v1_0::*;
use vulkanalia::vk::{DeviceV1_1, DeviceV1_2};

use crate::physical_device::{Features, Properties};
use crate::resources::{Buffer, BufferInfo, Fence, FenceState, MappableBuffer, Semaphore};
use crate::types::DeviceAddress;
use crate::Graphics;

#[derive(Clone)]
#[repr(transparent)]
pub struct WeakDevice(Weak<Inner>);

impl std::fmt::Debug for WeakDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.upgrade() {
            Some(device) => std::fmt::Debug::fmt(&device, f),
            None => write!(f, "Device({:?}, Destroyed)", self.0.as_ptr()),
        }
    }
}

impl WeakDevice {
    pub fn upgrade(&self) -> Option<Device> {
        self.0.upgrade().map(|inner| Device { inner })
    }

    pub fn is(&self, device: &Device) -> bool {
        std::ptr::eq(self.0.as_ptr(), &*device.inner)
    }
}

impl PartialEq<WeakDevice> for WeakDevice {
    fn eq(&self, other: &WeakDevice) -> bool {
        std::ptr::eq(self.0.as_ptr(), other.0.as_ptr())
    }
}

impl PartialEq<WeakDevice> for &WeakDevice {
    fn eq(&self, other: &WeakDevice) -> bool {
        std::ptr::eq(self.0.as_ptr(), other.0.as_ptr())
    }
}

#[derive(Clone)]
#[repr(transparent)]
pub struct Device {
    inner: Arc<Inner>,
}

impl Device {
    pub fn new(
        logical: vulkanalia::Device,
        physical: vk::PhysicalDevice,
        properties: Properties,
        features: Features,
        api_version: u32,
    ) -> Self {
        let allocator = Mutex::new(GpuAllocator::new(
            gpu_alloc::Config::i_am_prototyping(),
            map_memory_device_properties(&properties, &features),
        ));

        Self {
            inner: Arc::new(Inner {
                logical,
                physical,
                properties,
                features,
                api_version,
                allocator,
            }),
        }
    }

    pub fn graphics(&self) -> &'static Graphics {
        unsafe { Graphics::get_unchecked() }
    }

    pub fn logical(&self) -> &vulkanalia::Device {
        &self.inner.logical
    }

    pub fn physical(&self) -> vk::PhysicalDevice {
        self.inner.physical
    }

    pub fn properties(&self) -> &Properties {
        &self.inner.properties
    }

    pub fn features(&self) -> &Features {
        &self.inner.features
    }

    pub fn downgrade(&self) -> WeakDevice {
        WeakDevice(Arc::downgrade(&self.inner))
    }

    pub fn wait_idle(&self) -> Result<()> {
        self.inner.wait_idle()
    }

    pub fn create_semaphore(&self) -> Result<Semaphore> {
        let logical = &self.inner.logical;

        let info = vk::SemaphoreCreateInfo::builder();
        let handle = unsafe { logical.create_semaphore(&info, None) }?;

        tracing::debug!(semaphore = ?handle, "created semaphore");

        Ok(Semaphore::new(handle, self.downgrade()))
    }

    pub unsafe fn destroy_semaphore(&self, handle: vk::Semaphore) {
        self.inner.logical.destroy_semaphore(handle, None);
    }

    pub fn create_fence(&self) -> Result<Fence> {
        let logical = &self.inner.logical;

        let info = vk::FenceCreateInfo::builder();
        let handle = unsafe { logical.create_fence(&info, None) }?;

        tracing::debug!(fence = ?handle, "created fence");

        Ok(Fence::new(handle, self.downgrade()))
    }

    pub unsafe fn destroy_fence(&self, handle: vk::Fence) {
        self.inner.logical.destroy_fence(handle, None);
    }

    pub fn update_armed_fence_state(&self, fence: &mut Fence) -> Result<bool> {
        let status = unsafe { self.inner.logical.get_fence_status(fence.handle()) }?;
        match status {
            vk::SuccessCode::SUCCESS => {
                let _epoch = fence.set_signalled()?;
                // TODO: update epoch
                Ok(true)
            }
            vk::SuccessCode::NOT_READY => Ok(false),
            c => panic!("unexpected status code"),
        }
    }

    pub fn reset_fences(&self, fences: &mut [&mut Fence]) -> Result<()> {
        let handles = fences
            .iter_mut()
            .map(|fence| {
                if matches!(fence.state(), FenceState::Armed { .. }) {
                    match self.update_armed_fence_state(*fence) {
                        // Signalled -> ok
                        Ok(true) => {}
                        // Armed and not signalled yet -> error
                        Ok(false) => return Err(anyhow::anyhow!("armed fence cannot be reset")),
                        // Failed to check -> error
                        Err(e) => return Err(e),
                    }
                }
                Ok(fence.handle())
            })
            .collect::<Result<SmallVec<[_; 16]>>>()?;

        unsafe { self.inner.logical.reset_fences(&handles) }?;

        for fence in fences {
            fence.set_unsignalled()?;
        }

        Ok(())
    }

    pub fn wait_fences(&self, fences: &mut [&mut Fence], wait_all: bool) -> Result<()> {
        let handles = fences
            .iter()
            .filter_map(|fence| match fence.state() {
                // Waiting for an unarmed fence -> error (preventing deadlock)
                FenceState::Unsignalled => {
                    Some(Err(anyhow::anyhow!("waiting for an unarmed fence")))
                }
                // Waiting for an armed fence -> ok
                FenceState::Armed { .. } => Some(Ok(fence.handle())),
                // Already signalled fences could be skipped
                FenceState::Signalled => None,
            })
            .collect::<Result<SmallVec<[_; 16]>>>()?;

        if handles.is_empty() {
            return Ok(());
        }

        unsafe {
            self.inner
                .logical
                .wait_for_fences(&handles, wait_all, u64::MAX)
        }?;

        let all_signalled = wait_all || handles.len() == 1;
        for fence in fences {
            if all_signalled || self.update_armed_fence_state(fence)? {
                fence.set_signalled()?;
            }
        }

        // TODO: update epochs

        Ok(())
    }

    pub fn create_buffer(&self, info: BufferInfo) -> Result<Buffer> {
        self.create_buffer_impl(info, None)
            .map(MappableBuffer::freeze)
    }

    pub fn create_mappable_buffer(
        &self,
        info: BufferInfo,
        memory_usage: gpu_alloc::UsageFlags,
    ) -> Result<MappableBuffer> {
        self.create_buffer_impl(info, Some(memory_usage))
    }

    fn create_buffer_impl(
        &self,
        info: BufferInfo,
        memory_usage: Option<gpu_alloc::UsageFlags>,
    ) -> Result<MappableBuffer> {
        let logical = &self.inner.logical;

        let mut memory_usage = memory_usage.unwrap_or_else(gpu_alloc::UsageFlags::empty);
        let has_device_address = info
            .usage
            .contains(vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS);
        if has_device_address {
            anyhow::ensure!(
                self.inner.features.v1_2.buffer_device_address != 0,
                "`SHADER_DEVICE_ADDRESS` buffer usage requires `BufferDeviceAddress`
                feature"
            );
            memory_usage |= gpu_alloc::UsageFlags::DEVICE_ADDRESS;
        }

        let handle = {
            let info = vk::BufferCreateInfo::builder()
                .size(info.size)
                .usage(info.usage)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            unsafe { logical.create_buffer(&info, None)? }
        }
        .with_defer(|handle| unsafe { logical.destroy_buffer(handle, None) });

        let mut dedicated = vk::MemoryDedicatedRequirements::builder();
        let mut reqs = vk::MemoryRequirements2::builder().push_next(&mut dedicated);
        if self.graphics().vk1_1() {
            let info = vk::BufferMemoryRequirementsInfo2::builder().buffer(*handle);
            unsafe { logical.get_buffer_memory_requirements2(&info, &mut reqs) }
        } else {
            reqs.memory_requirements = unsafe { logical.get_buffer_memory_requirements(*handle) };
        }

        debug_assert!(reqs.memory_requirements.alignment.is_power_of_two());

        let block = {
            let request = gpu_alloc::Request {
                size: reqs.memory_requirements.size,
                align_mask: (reqs.memory_requirements.alignment - 1) | info.align,
                usage: memory_usage,
                memory_types: reqs.memory_requirements.memory_type_bits,
            };

            let dedicated = if dedicated.requires_dedicated_allocation != 0 {
                Some(gpu_alloc::Dedicated::Required)
            } else if dedicated.prefers_dedicated_allocation != 0 {
                Some(gpu_alloc::Dedicated::Preferred)
            } else {
                None
            };

            let logical = logical.as_memory_device();
            let mut allocator = self.inner.allocator.lock().unwrap();
            unsafe {
                match dedicated {
                    None => allocator.alloc(logical, request),
                    Some(dedicated) => allocator.alloc_with_dedicated(logical, request, dedicated),
                }
            }
        }?;

        unsafe { logical.bind_buffer_memory(*handle, *block.memory(), block.offset())? };

        let address = if has_device_address {
            let info = vk::BufferDeviceAddressInfo::builder().buffer(*handle);
            let address = unsafe { logical.get_buffer_device_address(&info) };
            Some(DeviceAddress::new(address).unwrap())
        } else {
            None
        };

        tracing::debug!(buffer = ?*handle, "created buffer");

        Ok(MappableBuffer::new(
            handle.disarm(),
            info,
            memory_usage,
            address,
            self.downgrade(),
            block,
        ))
    }

    pub unsafe fn destroy_buffer(&self, handle: vk::Buffer, block: MemoryBlock<vk::DeviceMemory>) {
        self.inner
            .allocator
            .lock()
            .unwrap()
            .dealloc(self.inner.logical.as_memory_device(), block);

        self.inner.logical.destroy_buffer(handle, None);
    }

    pub unsafe fn destroy_image(&self, handle: vk::Image, block: MemoryBlock<vk::DeviceMemory>) {
        self.inner
            .allocator
            .lock()
            .unwrap()
            .dealloc(self.inner.logical.as_memory_device(), block);

        self.inner.logical.destroy_image(handle, None)
    }
}

impl std::fmt::Debug for Device {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.inner.as_ref(), f)
    }
}

impl PartialEq<Device> for Device {
    fn eq(&self, other: &Device) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl PartialEq<Device> for &Device {
    fn eq(&self, other: &Device) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl PartialEq<WeakDevice> for Device {
    fn eq(&self, other: &WeakDevice) -> bool {
        std::ptr::eq(&*self.inner, other.0.as_ptr())
    }
}

impl PartialEq<WeakDevice> for &Device {
    fn eq(&self, other: &WeakDevice) -> bool {
        std::ptr::eq(&*self.inner, other.0.as_ptr())
    }
}

struct Inner {
    logical: vulkanalia::Device,
    physical: vk::PhysicalDevice,
    properties: Properties,
    features: Features,
    api_version: u32,
    allocator: Mutex<GpuAllocator<vk::DeviceMemory>>,
}

impl Inner {
    fn wait_idle(&self) -> Result<()> {
        // TODO: wait queues
        unsafe { self.logical.device_wait_idle()? };
        // TODO: reset queues?
        Ok(())
    }
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if f.alternate() {
            f.debug_struct("Device")
                .field("logical", &self.logical.handle())
                .field("physical", &self.physical)
                .finish()
        } else {
            std::fmt::Debug::fmt(&self.logical.handle(), f)
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.wait_idle();

        unsafe {
            self.allocator
                .get_mut()
                .unwrap()
                .cleanup(self.logical.as_memory_device());

            // TODO: destroy device?
        }
    }
}

fn map_memory_device_properties(
    propertis: &Properties,
    features: &Features,
) -> gpu_alloc::DeviceProperties<'static> {
    let memory = &propertis.memory;
    let limits = &propertis.v1_0.limits;

    let mut max_memory_allocation_size = propertis.v1_1.max_memory_allocation_size;
    if max_memory_allocation_size == 0 {
        max_memory_allocation_size = u64::MAX;
    }

    gpu_alloc::DeviceProperties {
        memory_types: memory.memory_types[..memory.memory_type_count as usize]
            .iter()
            .map(|ty| gpu_alloc::MemoryType {
                heap: ty.heap_index,
                props: gpu_alloc_vulkanalia::memory_properties_from(ty.property_flags),
            })
            .collect(),
        memory_heaps: memory.memory_heaps[..memory.memory_heap_count as usize]
            .iter()
            .map(|heap| gpu_alloc::MemoryHeap { size: heap.size })
            .collect(),
        max_memory_allocation_count: limits.max_memory_allocation_count,
        max_memory_allocation_size,
        non_coherent_atom_size: limits.non_coherent_atom_size,
        buffer_device_address: features.v1_2.buffer_device_address != 0,
    }
}
