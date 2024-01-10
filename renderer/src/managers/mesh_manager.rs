use std::ops::Range;

use anyhow::Result;
use range_alloc::RangeAllocator;

use crate::types::{Mesh, MeshHandle, VertexAttributeKind};

pub struct MeshManager {
    buffers: MeshBuffers,
    vertex_alloc: RangeAllocator<u64>,
    index_alloc: RangeAllocator<u64>,
    registry: Vec<Option<GpuMesh>>,
}

impl MeshManager {
    pub fn new(device: &gfx::Device) -> Result<Self> {
        const INITIAL_VERTICES_CAPACITY: u64 = 1 << 16;
        const INITIAL_INDEX_COUNT: u64 = 1 << 16;

        let buffers = MeshBuffers::new(device, INITIAL_INDEX_COUNT, INITIAL_INDEX_COUNT)?;
        let vertex_alloc = RangeAllocator::new(0..INITIAL_VERTICES_CAPACITY);
        let index_alloc = RangeAllocator::new(0..INITIAL_INDEX_COUNT);

        Ok(Self {
            buffers,
            vertex_alloc,
            index_alloc,
            registry: Default::default(),
        })
    }

    pub fn buffers(&self) -> &MeshBuffers {
        &self.buffers
    }

    pub fn upload_mesh(
        &mut self,
        device: &gfx::Device,
        encoder: &mut gfx::Encoder,
        mesh: &Mesh,
    ) -> Result<GpuMesh> {
        let vertex_count = mesh.vertex_count;
        let index_count = mesh.indices.len();
        if vertex_count == 0 || index_count == 0 {
            return Ok(GpuMesh::new_empty());
        }

        let mut vertex_attribute_ranges = Vec::with_capacity(mesh.attribute_data.len());
        let mut vertex_attribute_copies = Vec::with_capacity(vertex_attribute_ranges.len());
        let indices_range;
        let indices_copy;

        let staging_buffer = {
            // Create a host-coherent staging buffer
            let total_attribute_size = mesh
                .attribute_data
                .iter()
                .map(|a| a.byte_len())
                .sum::<usize>();
            let total_index_size = index_count * (INDEX_SIZE as usize);

            let mut staging_buffer = device.create_mappable_buffer(
                gfx::BufferInfo {
                    align: VERTEX_ALIGN_MASK.max(INDEX_ALIGN_MASK),
                    size: (total_attribute_size + total_index_size) as u64,
                    usage: gfx::BufferUsage::TRANSFER_SRC,
                },
                gfx::MemoryUsage::UPLOAD | gfx::MemoryUsage::TRANSIENT,
            )?;

            // Map staging buffer to host memory
            let staging_buffer_data = device.map_memory(
                &mut staging_buffer,
                0,
                (total_attribute_size + total_index_size) as _,
            )?;
            let staging_buffer_data = staging_buffer_data.as_mut_ptr();
            let mut staging_buffer_offset = 0;

            // Allocate ranges for vertex attributes
            for attribute in &mesh.attribute_data {
                let data = attribute.untyped_data();
                let len = data.len();

                // SAFETY: `staging_buffer_data` is a valid pointer to a slice of at least `len` bytes.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        data.as_ptr(),
                        staging_buffer_data.add(staging_buffer_offset).cast(),
                        len,
                    );
                }

                let range = self.alloc_range_for_vertices(device, encoder, len as _)?;
                vertex_attribute_copies.push(gfx::BufferCopy {
                    src_offset: staging_buffer_offset as u64,
                    dst_offset: range.start,
                    size: range.end - range.start,
                });
                vertex_attribute_ranges.push((attribute.kind(), range));

                staging_buffer_offset += len;
            }

            // Allocate range for indices

            // SAFETY: `staging_buffer_data` is a valid pointer to a slice with
            // the exact remaining capacity required for `mesh.indices`.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    mesh.indices.as_ptr().cast::<u8>(),
                    staging_buffer_data.add(staging_buffer_offset).cast(),
                    std::mem::size_of_val::<[_]>(mesh.indices.as_slice()),
                );
            }

            indices_range = self.alloc_range_for_indices(device, encoder, index_count)?;
            indices_copy = gfx::BufferCopy {
                src_offset: staging_buffer_offset as u64,
                dst_offset: indices_range.start,
                size: indices_range.end - indices_range.start,
            };

            // Unmap and freeze staging buffer
            device.unmap_memory(&mut staging_buffer);
            staging_buffer.freeze()
        };

        // Encode copy commands
        encoder.copy_buffer(
            &staging_buffer,
            &self.buffers.vertices,
            &vertex_attribute_copies,
        );
        encoder.copy_buffer(
            &staging_buffer,
            &self.buffers.indices,
            std::slice::from_ref(&indices_copy),
        );

        // Done
        Ok(GpuMesh {
            vertex_count,
            vertex_attribute_ranges,
            indices_range,
        })
    }

    pub fn insert(&mut self, handle: &MeshHandle, mesh: GpuMesh) {
        let index = handle.index();
        if index >= self.registry.len() {
            self.registry.resize_with(index + 1, || None);
        }
        self.registry[index] = Some(mesh);
    }

    pub fn remove(&mut self, handle: &MeshHandle) {
        let index = handle.index();
        let mesh = self.registry[index].take().expect("handle must be valid");

        for (_, range) in mesh.vertex_attribute_ranges {
            if !range.is_empty() {
                self.vertex_alloc.free_range(range);
            }
        }

        if !mesh.indices_range.is_empty() {
            self.index_alloc.free_range(mesh.indices_range);
        }
    }

    fn alloc_range_for_vertices(
        &mut self,
        device: &gfx::Device,
        encoder: &mut gfx::Encoder,
        size: u64,
    ) -> Result<Range<u64>> {
        match self.vertex_alloc.allocate_range(size) {
            Ok(range) => Ok(range),
            Err(_) => {
                self.realloc(device, encoder, size, 0)?;
                Ok(self
                    .vertex_alloc
                    .allocate_range(size)
                    .expect("`vertex_alloc` must grow after `realloc`"))
            }
        }
    }

    fn alloc_range_for_indices(
        &mut self,
        device: &gfx::Device,
        encoder: &mut gfx::Encoder,
        count: usize,
    ) -> Result<Range<u64>> {
        match self.index_alloc.allocate_range(count as _) {
            Ok(range) => Ok(range),
            Err(_) => {
                self.realloc(device, encoder, 0, count as _)?;
                Ok(self
                    .index_alloc
                    .allocate_range(count as _)
                    .expect("`index_alloc` must grow after `realloc`"))
            }
        }
    }

    fn realloc(
        &mut self,
        device: &gfx::Device,
        encoder: &mut gfx::Encoder,
        additional_vertices_capacity: u64,
        additional_index_count: u64,
    ) -> Result<()> {
        let update_vertices = additional_vertices_capacity > 0;
        let update_indices = additional_index_count > 0;
        if !update_vertices && !update_indices {
            return Ok(());
        }

        let max_buffer_size = device.limits().max_storage_buffer_range as u64;

        // Make vertices buffer if needed
        let current_vertices_size = self.index_alloc.initial_range().end;
        let new_vertices = if update_vertices {
            let new_vertices_size = current_vertices_size
                .checked_add(additional_vertices_capacity)
                .and_then(|size| size.checked_next_power_of_two())
                .expect("too many vertices")
                .min(max_buffer_size);

            anyhow::ensure!(
                new_vertices_size > current_vertices_size,
                "max vertex buffer size exceeded ({max_buffer_size} bytes)"
            );

            Some((make_vertices(device, new_vertices_size)?, new_vertices_size))
        } else {
            None
        };

        // Make indices buffer if needed
        let current_index_count = self.index_alloc.initial_range().end;
        let current_indices_size = current_index_count.saturating_mul(INDEX_SIZE);
        let new_indices = if update_indices {
            let new_indices_size = current_indices_size
                .checked_add(additional_index_count.saturating_mul(INDEX_SIZE))
                .and_then(|size| size.checked_next_power_of_two())
                .expect("too many indices")
                .min(max_buffer_size);

            anyhow::ensure!(
                !update_indices || new_indices_size > current_indices_size,
                "max index buffer size exceeded ({max_buffer_size} bytes)"
            );
            anyhow::ensure!(
                new_indices_size % INDEX_SIZE == 0,
                "unaligned index buffer size ({new_indices_size} bytes, must be multiple of {INDEX_SIZE})"
            );

            Some((make_indices(device, new_indices_size)?, new_indices_size))
        } else {
            None
        };

        // Update vertex buffer
        if let Some((new_vertices, new_vertices_size)) = new_vertices {
            let old_buffer = std::mem::replace(&mut self.buffers.vertices, new_vertices);
            self.vertex_alloc.grow_to(new_vertices_size);
            encoder.copy_buffer(
                &old_buffer,
                &self.buffers.vertices,
                &[gfx::BufferCopy {
                    src_offset: 0,
                    dst_offset: 0,
                    size: current_vertices_size,
                }],
            );
        }

        // Update index buffer
        if let Some((new_indices, new_indices_size)) = new_indices {
            let old_buffer = std::mem::replace(&mut self.buffers.indices, new_indices);
            self.index_alloc.grow_to(new_indices_size / INDEX_SIZE);
            encoder.copy_buffer(
                &old_buffer,
                &self.buffers.indices,
                &[gfx::BufferCopy {
                    src_offset: 0,
                    dst_offset: 0,
                    size: current_indices_size,
                }],
            );
        }

        Ok(())
    }
}

pub struct GpuMesh {
    pub vertex_count: u32,
    pub vertex_attribute_ranges: Vec<(VertexAttributeKind, Range<u64>)>,
    pub indices_range: Range<u64>,
}

impl GpuMesh {
    pub fn new_empty() -> Self {
        Self {
            vertex_count: 0,
            vertex_attribute_ranges: Default::default(),
            indices_range: 0..0,
        }
    }

    pub fn attributes(&self) -> impl Iterator<Item = VertexAttributeKind> + '_ {
        self.vertex_attribute_ranges
            .iter()
            .map(|(component, _)| *component)
    }

    pub fn get_attribute_range(&self, component: VertexAttributeKind) -> Option<Range<u64>> {
        self.vertex_attribute_ranges
            .iter()
            .find_map(|(c, range)| (*c == component).then_some(range.clone()))
    }

    pub fn indices(&self) -> Range<u64> {
        self.indices_range.clone()
    }
}

pub struct MeshBuffers {
    vertices: gfx::Buffer,
    indices: gfx::Buffer,
}

impl MeshBuffers {
    fn new(device: &gfx::Device, vertices_capacity: u64, index_count: u64) -> Result<Self> {
        Ok(Self {
            vertices: make_vertices(device, vertices_capacity)?,
            indices: make_indices(device, index_count * INDEX_SIZE)?,
        })
    }

    pub fn bind_index_buffer(&self, encoder: &mut gfx::Encoder) {
        encoder.bind_index_buffer(&self.indices, 0, INDEX_TYPE);
    }
}

fn make_vertices(device: &gfx::Device, size: u64) -> Result<gfx::Buffer> {
    device.create_buffer(gfx::BufferInfo {
        align: VERTEX_ALIGN_MASK,
        size,
        usage: gfx::BufferUsage::TRANSFER_DST
            | gfx::BufferUsage::TRANSFER_SRC
            | gfx::BufferUsage::STORAGE,
    })
}

fn make_indices(device: &gfx::Device, size: u64) -> Result<gfx::Buffer> {
    device.create_buffer(gfx::BufferInfo {
        align: INDEX_ALIGN_MASK,
        size,
        usage: gfx::BufferUsage::TRANSFER_DST
            | gfx::BufferUsage::TRANSFER_SRC
            | gfx::BufferUsage::STORAGE
            | gfx::BufferUsage::INDEX,
    })
}

const VERTEX_ALIGN_MASK: u64 = 0b1111;
const INDEX_ALIGN_MASK: u64 = 0b11;
const INDEX_TYPE: gfx::IndexType = gfx::IndexType::U32;
const INDEX_SIZE: u64 = INDEX_TYPE.index_size() as u64;
