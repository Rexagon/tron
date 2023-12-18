use anyhow::{Context, Result};
use shared::{FastHashMap, FastHashSet};
use vulkanalia::prelude::v1_0::*;
use vulkanalia::vk::InstanceV1_1;

use crate::queue::QueuesQuery;
use crate::Graphics;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum Feature {
    BufferDeviceAddress,
    DisplayTiming,
    ScalarBlockLayout,
    SurfacePresentation,
}

#[derive(Debug)]
pub struct PhysicalDevice {
    handle: vk::PhysicalDevice,
    properties: Properties,
    features: Features,
}

impl PhysicalDevice {
    pub unsafe fn new(handle: vk::PhysicalDevice) -> Self {
        let (properties, features) = collect_info(handle);
        PhysicalDevice {
            handle,
            properties,
            features,
        }
    }

    pub fn graphics(&self) -> &'static Graphics {
        // `PhysicalDevice` can only be created from `Graphics` instance
        unsafe { Graphics::get_unchecked() }
    }

    pub fn create_device<Q>(self, features: &[Feature], queues: Q) -> Result<()>
    where
        Q: QueuesQuery,
    {
        let graphics = self.graphics();

        let (queue_families, queues_query_state) = queues.query(&self.properties.queue_families)?;
        let queue_families = queue_families.as_ref();

        let mut device_create_info = vk::DeviceCreateInfo::builder();

        // Collect queries
        let mut priorities = FastHashMap::<usize, Vec<f32>>::default();
        for &(family_idx, count) in queue_families {
            let Some(family) = self.properties.queue_families.get(family_idx) else {
                anyhow::bail!("requested queue family not found: {family_idx}");
            };
            let priorities = priorities.entry(family_idx).or_default();
            let queue_count = priorities.len() + count;
            anyhow::ensure!(
                queue_count <= family.queue_count as usize,
                "too many queues requested for the queue family {family_idx}: {queue_count} out of {}",
                family.queue_count
            );

            priorities.resize(queue_count, 1.0f32);
        }

        let queue_create_infos = priorities
            .iter()
            .map(|(&family_idx, priorities)| {
                vk::DeviceQueueCreateInfo::builder()
                    .queue_family_index(family_idx as u32)
                    .queue_priorities(priorities)
            })
            .collect::<Vec<_>>();

        device_create_info = device_create_info.queue_create_infos(&queue_create_infos);

        // Collect requested features
        let mut requested_features = features.iter().copied().collect::<FastHashSet<_>>();

        let mut extensions = Vec::new();
        let mut require_ext = {
            let supported_extensions = &self.properties.extensions;
            |ext: &vk::Extension| -> Result<()> {
                let ext = &ext.name;
                let supported = supported_extensions.contains(ext);
                anyhow::ensure!(supported, "extension {ext} is missing");
                if extensions.contains(&ext.as_ptr()) {
                    extensions.push(ext.as_ptr());
                }
                Ok(())
            }
        };

        let mut features_v1_0 = vk::PhysicalDeviceFeatures::builder();
        let mut features_v1_2 = vk::PhysicalDeviceVulkan12Features::builder();
        let mut features_sbl = vk::PhysicalDeviceScalarBlockLayoutFeatures::builder();
        let mut features_bda = vk::PhysicalDeviceBufferAddressFeaturesEXT::builder();

        let mut include_features_v1_2 = false;
        let mut include_features_sbl = false;
        let mut include_features_bda = false;

        // === Begin fill feature requirements ===
        if requested_features.remove(&Feature::SurfacePresentation) {
            require_ext(&vk::KHR_SWAPCHAIN_EXTENSION)
                .context("SurfacePresentation feature is required but not supported")?;
        }

        if requested_features.remove(&Feature::ScalarBlockLayout) {
            anyhow::ensure!(
                self.features.v1_2.scalar_block_layout != 0,
                "ScalarBlockLayout feature is required but not supported"
            );
            features_v1_2.scalar_block_layout = 1;
            include_features_v1_2 = true;
            features_sbl.scalar_block_layout = 1;
            include_features_sbl = true;
        }

        if requested_features.remove(&Feature::BufferDeviceAddress) {
            anyhow::ensure!(
                self.features.v1_2.buffer_device_address != 0,
                "BufferDeviceAddress feature is required but not supported"
            );
            features_v1_2.buffer_device_address = 1;
            include_features_v1_2 = true;
            features_bda.buffer_device_address = 1;
            include_features_bda = true;
        }

        if requested_features.remove(&Feature::DisplayTiming) {
            require_ext(&vk::GOOGLE_DISPLAY_TIMING_EXTENSION)
                .context("DisplayTiming feature is required but not supported")?;
        }
        // === End fill feature requirements ===

        device_create_info = device_create_info.enabled_features(&features_v1_0);

        // Prevent requiring duplicate blocks
        if graphics.vk1_2() {
            include_features_sbl = false;
            include_features_bda = false;
        } else {
            include_features_v1_2 = false;
        }

        // Require requested blocks
        if include_features_v1_2 {
            device_create_info = device_create_info.push_next(&mut features_v1_2);
        }
        if include_features_sbl {
            require_ext(&vk::EXT_SCALAR_BLOCK_LAYOUT_EXTENSION)?;
            device_create_info = device_create_info.push_next(&mut features_sbl);
        }
        if include_features_bda {
            require_ext(&vk::KHR_BUFFER_DEVICE_ADDRESS_EXTENSION)?;
            device_create_info = device_create_info.push_next(&mut features_bda);
        }

        // Ensure all required features are supported
        anyhow::ensure!(
            requested_features.is_empty(),
            "some features are required but not supported: {}",
            requested_features
                .into_iter()
                .map(|f| format!("{f:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        );

        // Create device
        let device = unsafe {
            graphics
                .instance()
                .create_device(self.handle, &device_create_info, None)?
        };

        Ok(())
    }
}

#[derive(Debug)]
pub struct Properties {
    pub extensions: FastHashSet<vk::ExtensionName>,
    pub queue_families: Vec<vk::QueueFamilyProperties>,
    pub memory: vk::PhysicalDeviceMemoryProperties,
    pub v1_0: vk::PhysicalDeviceProperties,
    pub v1_1: vk::PhysicalDeviceVulkan11Properties,
    pub v1_2: vk::PhysicalDeviceVulkan12Properties,
    pub v1_3: vk::PhysicalDeviceVulkan13Properties,
}

unsafe impl Sync for Properties {}
unsafe impl Send for Properties {}

#[derive(Debug)]
pub struct Features {
    pub v1_0: vk::PhysicalDeviceFeatures,
    pub v1_1: vk::PhysicalDeviceVulkan11Features,
    pub v1_2: vk::PhysicalDeviceVulkan12Features,
    pub v1_3: vk::PhysicalDeviceVulkan13Features,
}

unsafe impl Sync for Features {}
unsafe impl Send for Features {}

unsafe fn collect_info(handle: vk::PhysicalDevice) -> (Properties, Features) {
    let graphics = Graphics::get_unchecked();
    let instance = graphics.instance();
    let (vk1_1, vk1_2, vk1_3) = (graphics.vk1_1(), graphics.vk1_2(), graphics.vk1_3());

    let extensions = instance
        .enumerate_device_extension_properties(handle, None)
        .unwrap()
        .into_iter()
        .map(|item| item.extension_name)
        .collect::<FastHashSet<_>>();
    let has_device_ext = |ext: &vk::Extension| -> bool { extensions.contains(&ext.name) };

    let properties_v1_0;
    let mut properties_v1_1 = vk::PhysicalDeviceVulkan11Properties::builder();
    let mut properties_v1_2 = vk::PhysicalDeviceVulkan12Properties::builder();
    let mut properties_v1_3 = vk::PhysicalDeviceVulkan13Properties::builder();
    let mut properties_di = vk::PhysicalDeviceDescriptorIndexingProperties::builder();
    let mut properties_mt3 = vk::PhysicalDeviceMaintenance3Properties::builder();

    let features_v1_0;
    let mut features_v1_1 = vk::PhysicalDeviceVulkan11Features::builder();
    let mut features_v1_2 = vk::PhysicalDeviceVulkan12Features::builder();
    let mut features_v1_3 = vk::PhysicalDeviceVulkan13Features::builder();
    let mut features_di = vk::PhysicalDeviceDescriptorIndexingFeatures::builder();
    let mut features_sbl = vk::PhysicalDeviceScalarBlockLayoutFeaturesEXT::builder();
    let mut features_bda = vk::PhysicalDeviceBufferAddressFeaturesEXT::builder();

    // Query info
    if vk1_1
        || instance
            .extensions()
            .contains(&vk::KHR_GET_PHYSICAL_DEVICE_PROPERTIES2_EXTENSION.name)
    {
        let mut properties2 = vk::PhysicalDeviceProperties2::builder();
        let mut features2 = vk::PhysicalDeviceFeatures2::builder();

        // Core properties and features
        if vk1_1 {
            properties2 = properties2.push_next(&mut properties_v1_1);
            features2 = features2.push_next(&mut features_v1_1);
        }
        if vk1_2 {
            properties2 = properties2.push_next(&mut properties_v1_2);
            features2 = features2.push_next(&mut features_v1_2);
        }
        if vk1_3 {
            properties2 = properties2.push_next(&mut properties_v1_3);
            features2 = features2.push_next(&mut features_v1_3);
        }

        // Extension properties and features
        if !vk1_1 && has_device_ext(&vk::KHR_MAINTENANCE3_EXTENSION) {
            properties2 = properties2.push_next(&mut properties_mt3);
        }
        if !vk1_2 && has_device_ext(&vk::EXT_DESCRIPTOR_INDEXING_EXTENSION) {
            properties2 = properties2.push_next(&mut properties_di);
            features2 = features2.push_next(&mut features_di);
        }
        if !vk1_2 && has_device_ext(&vk::EXT_SCALAR_BLOCK_LAYOUT_EXTENSION) {
            features2 = features2.push_next(&mut features_sbl);
        }
        if !vk1_2 && has_device_ext(&vk::KHR_BUFFER_DEVICE_ADDRESS_EXTENSION) {
            features2 = features2.push_next(&mut features_bda);
        }

        // Query extended info
        instance.get_physical_device_properties2(handle, &mut properties2);
        instance.get_physical_device_features2(handle, &mut features2);

        properties_di.next = std::ptr::null_mut();
        properties_mt3.next = std::ptr::null_mut();
        properties_v1_3.next = std::ptr::null_mut();
        properties_v1_2.next = std::ptr::null_mut();
        properties_v1_1.next = std::ptr::null_mut();
        properties_v1_0 = properties2.properties;

        features_bda.next = std::ptr::null_mut();
        features_sbl.next = std::ptr::null_mut();
        features_di.next = std::ptr::null_mut();
        features_v1_3.next = std::ptr::null_mut();
        features_v1_2.next = std::ptr::null_mut();
        features_v1_1.next = std::ptr::null_mut();
        features_v1_0 = features2.features;
    } else {
        // Query basic info
        properties_v1_0 = instance.get_physical_device_properties(handle);
        features_v1_0 = instance.get_physical_device_features(handle);
    }

    // Other info
    let queue_families = instance.get_physical_device_queue_family_properties(handle);
    let memory_properties = instance.get_physical_device_memory_properties(handle);

    // Map extensions to core
    if !vk1_1 && has_device_ext(&vk::KHR_MAINTENANCE3_EXTENSION) {
        properties_v1_1.max_per_set_descriptors = properties_mt3.max_per_set_descriptors;
        properties_v1_1.max_memory_allocation_size = properties_mt3.max_memory_allocation_size;
    }
    if !vk1_2 && has_device_ext(&vk::EXT_DESCRIPTOR_INDEXING_EXTENSION) {
        properties_v1_2.max_update_after_bind_descriptors_in_all_pools =
            properties_di.max_update_after_bind_descriptors_in_all_pools;
        properties_v1_2.shader_uniform_buffer_array_non_uniform_indexing_native =
            properties_di.shader_uniform_buffer_array_non_uniform_indexing_native;
        properties_v1_2.shader_sampled_image_array_non_uniform_indexing_native =
            properties_di.shader_sampled_image_array_non_uniform_indexing_native;
        properties_v1_2.shader_storage_buffer_array_non_uniform_indexing_native =
            properties_di.shader_storage_buffer_array_non_uniform_indexing_native;
        properties_v1_2.shader_storage_image_array_non_uniform_indexing_native =
            properties_di.shader_storage_image_array_non_uniform_indexing_native;
        properties_v1_2.shader_input_attachment_array_non_uniform_indexing_native =
            properties_di.shader_input_attachment_array_non_uniform_indexing_native;
        properties_v1_2.robust_buffer_access_update_after_bind =
            properties_di.robust_buffer_access_update_after_bind;
        properties_v1_2.quad_divergent_implicit_lod = properties_di.quad_divergent_implicit_lod;
        properties_v1_2.max_per_stage_descriptor_update_after_bind_samplers =
            properties_di.max_per_stage_descriptor_update_after_bind_samplers;
        properties_v1_2.max_per_stage_descriptor_update_after_bind_uniform_buffers =
            properties_di.max_per_stage_descriptor_update_after_bind_uniform_buffers;
        properties_v1_2.max_per_stage_descriptor_update_after_bind_storage_buffers =
            properties_di.max_per_stage_descriptor_update_after_bind_storage_buffers;
        properties_v1_2.max_per_stage_descriptor_update_after_bind_sampled_images =
            properties_di.max_per_stage_descriptor_update_after_bind_sampled_images;
        properties_v1_2.max_per_stage_descriptor_update_after_bind_storage_images =
            properties_di.max_per_stage_descriptor_update_after_bind_storage_images;
        properties_v1_2.max_per_stage_descriptor_update_after_bind_input_attachments =
            properties_di.max_per_stage_descriptor_update_after_bind_input_attachments;
        properties_v1_2.max_per_stage_update_after_bind_resources =
            properties_di.max_per_stage_update_after_bind_resources;
        properties_v1_2.max_descriptor_set_update_after_bind_samplers =
            properties_di.max_descriptor_set_update_after_bind_samplers;
        properties_v1_2.max_descriptor_set_update_after_bind_uniform_buffers =
            properties_di.max_descriptor_set_update_after_bind_uniform_buffers;
        properties_v1_2.max_descriptor_set_update_after_bind_uniform_buffers_dynamic =
            properties_di.max_descriptor_set_update_after_bind_uniform_buffers_dynamic;
        properties_v1_2.max_descriptor_set_update_after_bind_storage_buffers =
            properties_di.max_descriptor_set_update_after_bind_storage_buffers;
        properties_v1_2.max_descriptor_set_update_after_bind_storage_buffers_dynamic =
            properties_di.max_descriptor_set_update_after_bind_storage_buffers_dynamic;
        properties_v1_2.max_descriptor_set_update_after_bind_sampled_images =
            properties_di.max_descriptor_set_update_after_bind_sampled_images;
        properties_v1_2.max_descriptor_set_update_after_bind_storage_images =
            properties_di.max_descriptor_set_update_after_bind_storage_images;
        properties_v1_2.max_descriptor_set_update_after_bind_input_attachments =
            properties_di.max_descriptor_set_update_after_bind_input_attachments;

        features_v1_2.shader_input_attachment_array_dynamic_indexing =
            features_di.shader_input_attachment_array_dynamic_indexing;
        features_v1_2.shader_uniform_texel_buffer_array_dynamic_indexing =
            features_di.shader_uniform_texel_buffer_array_dynamic_indexing;
        features_v1_2.shader_storage_texel_buffer_array_dynamic_indexing =
            features_di.shader_storage_texel_buffer_array_dynamic_indexing;
        features_v1_2.shader_uniform_buffer_array_non_uniform_indexing =
            features_di.shader_uniform_buffer_array_non_uniform_indexing;
        features_v1_2.shader_sampled_image_array_non_uniform_indexing =
            features_di.shader_sampled_image_array_non_uniform_indexing;
        features_v1_2.shader_storage_buffer_array_non_uniform_indexing =
            features_di.shader_storage_buffer_array_non_uniform_indexing;
        features_v1_2.shader_storage_image_array_non_uniform_indexing =
            features_di.shader_storage_image_array_non_uniform_indexing;
        features_v1_2.shader_input_attachment_array_non_uniform_indexing =
            features_di.shader_input_attachment_array_non_uniform_indexing;
        features_v1_2.shader_uniform_texel_buffer_array_non_uniform_indexing =
            features_di.shader_uniform_texel_buffer_array_non_uniform_indexing;
        features_v1_2.shader_storage_texel_buffer_array_non_uniform_indexing =
            features_di.shader_storage_texel_buffer_array_non_uniform_indexing;
        features_v1_2.descriptor_binding_uniform_buffer_update_after_bind =
            features_di.descriptor_binding_uniform_buffer_update_after_bind;
        features_v1_2.descriptor_binding_sampled_image_update_after_bind =
            features_di.descriptor_binding_sampled_image_update_after_bind;
        features_v1_2.descriptor_binding_storage_image_update_after_bind =
            features_di.descriptor_binding_storage_image_update_after_bind;
        features_v1_2.descriptor_binding_storage_buffer_update_after_bind =
            features_di.descriptor_binding_storage_buffer_update_after_bind;
        features_v1_2.descriptor_binding_uniform_texel_buffer_update_after_bind =
            features_di.descriptor_binding_uniform_texel_buffer_update_after_bind;
        features_v1_2.descriptor_binding_storage_texel_buffer_update_after_bind =
            features_di.descriptor_binding_storage_texel_buffer_update_after_bind;
        features_v1_2.descriptor_binding_update_unused_while_pending =
            features_di.descriptor_binding_update_unused_while_pending;
        features_v1_2.descriptor_binding_partially_bound =
            features_di.descriptor_binding_partially_bound;
        features_v1_2.descriptor_binding_variable_descriptor_count =
            features_di.descriptor_binding_variable_descriptor_count;
        features_v1_2.runtime_descriptor_array = features_di.runtime_descriptor_array;
    }
    if !vk1_2 && has_device_ext(&vk::EXT_SCALAR_BLOCK_LAYOUT_EXTENSION) {
        features_v1_2.scalar_block_layout = features_sbl.scalar_block_layout;
    }
    if !vk1_2 && has_device_ext(&vk::KHR_BUFFER_DEVICE_ADDRESS_EXTENSION) {
        features_v1_2.buffer_device_address = features_bda.buffer_device_address;
        features_v1_2.buffer_device_address_capture_replay =
            features_bda.buffer_device_address_capture_replay;
        features_v1_2.buffer_device_address_multi_device =
            features_bda.buffer_device_address_multi_device;
    }

    let properties = Properties {
        extensions,
        queue_families,
        memory: memory_properties,
        v1_0: properties_v1_0,
        v1_1: properties_v1_1.build(),
        v1_2: properties_v1_2.build(),
        v1_3: properties_v1_3.build(),
    };
    let features = Features {
        v1_0: features_v1_0,
        v1_1: features_v1_1.build(),
        v1_2: features_v1_2.build(),
        v1_3: features_v1_3.build(),
    };
    (properties, features)
}
