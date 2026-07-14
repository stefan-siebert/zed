//! Zero-copy import of DMABuf video frames as wgpu textures.
//!
//! A decoder (GStreamer/VA-API) exports its decoded NV12 surface as a DMABuf;
//! this module wraps it in a Vulkan image created with
//! `VK_EXT_image_drm_format_modifier` (explicit tiling layout) and
//! `VK_EXT_external_memory_dma_buf` (fd import), then hands it to wgpu as an
//! `NV12` texture. The renderer samples its luma/chroma planes through
//! `TextureAspect::Plane0/Plane1` views with the existing `fs_surface`
//! YCbCr shader — the frame never leaves the GPU.

use anyhow::{Context as _, Result, bail};
use ash::vk;
use gpui::{DRM_FOURCC_NV12, DmabufFrame};
use std::os::fd::{BorrowedFd, IntoRawFd};
use wgpu::hal::api::Vulkan;

/// Import `frame` as an NV12 wgpu texture. The returned texture owns a
/// `dup()`ed fd and a clone of the frame's owner; both are released when
/// wgpu destroys the texture (after all GPU work using it completed).
pub(crate) fn import_dmabuf_texture(
    device: &wgpu::Device,
    frame: &DmabufFrame,
) -> Result<wgpu::Texture> {
    if frame.fourcc != DRM_FOURCC_NV12 {
        bail!("unsupported dmabuf fourcc {:#010x} (only NV12)", frame.fourcc);
    }
    if frame.planes.is_empty() || frame.planes.len() > 2 {
        bail!("unsupported dmabuf plane count {}", frame.planes.len());
    }
    let fd = frame
        .planes
        .first()
        .context("dmabuf frame without planes")?
        .fd;
    if frame.planes.iter().any(|plane| plane.fd != fd) {
        // Single-memory import only; VA-API exports one fd for both planes.
        bail!("multi-fd dmabuf frames are not supported");
    }

    let size = wgpu::Extent3d {
        width: frame.width,
        height: frame.height,
        depth_or_array_layers: 1,
    };

    let hal_texture = {
        let hal_device = unsafe { device.as_hal::<Vulkan>() }
            .context("dmabuf import requires the Vulkan backend")?;
        let raw_device = hal_device.raw_device().clone();

        let plane_layouts: Vec<vk::SubresourceLayout> = frame
            .planes
            .iter()
            .map(|plane| {
                vk::SubresourceLayout::default()
                    .offset(u64::from(plane.offset))
                    .row_pitch(u64::from(plane.stride))
            })
            .collect();

        let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(frame.modifier)
            .plane_layouts(&plane_layouts);
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .extent(vk::Extent3D {
                width: frame.width,
                height: frame.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_info)
            .push_next(&mut modifier_info);

        // SAFETY: create info describes the producer's layout verbatim; the
        // image is destroyed in the drop callback below (or on error here).
        let image = unsafe { raw_device.create_image(&image_info, None) }
            .context("vkCreateImage(dmabuf) failed")?;
        let destroy_image = || unsafe { raw_device.destroy_image(image, None) };

        let requirements = unsafe { raw_device.get_image_memory_requirements(image) };
        let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
        {
            let loader = ash::khr::external_memory_fd::Device::new(
                hal_device.shared_instance().raw_instance(),
                &raw_device,
            );
            // SAFETY: queries fd properties; does not take fd ownership.
            let queried = unsafe {
                loader.get_memory_fd_properties(
                    vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                    fd,
                    &mut fd_properties,
                )
            };
            if let Err(err) = queried {
                destroy_image();
                bail!("vkGetMemoryFdPropertiesKHR failed: {err}");
            }
        }

        let type_bits = requirements.memory_type_bits & fd_properties.memory_type_bits;
        if type_bits == 0 {
            destroy_image();
            bail!("no compatible memory type for dmabuf import");
        }
        let memory_type_index = type_bits.trailing_zeros();

        // Vulkan takes ownership of the imported fd — hand it a duplicate;
        // the producer's fd stays with `frame.owner`.
        let imported_fd = match unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned() {
            Ok(owned) => owned,
            Err(err) => {
                destroy_image();
                bail!("dup(dmabuf fd) failed: {err}");
            }
        };

        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(imported_fd.into_raw_fd());
        let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut import_info)
            .push_next(&mut dedicated_info);

        // SAFETY: imports the dup'ed fd (consumed even on failure, per spec).
        let memory = match unsafe { raw_device.allocate_memory(&allocate_info, None) } {
            Ok(memory) => memory,
            Err(err) => {
                destroy_image();
                bail!("vkAllocateMemory(dmabuf import) failed: {err}");
            }
        };
        // SAFETY: freshly created image + memory, offset 0 (dedicated).
        if let Err(err) = unsafe { raw_device.bind_image_memory(image, memory, 0) } {
            destroy_image();
            // SAFETY: memory was just allocated and never bound.
            unsafe { raw_device.free_memory(memory, None) };
            bail!("vkBindImageMemory(dmabuf) failed: {err}");
        }

        let hal_descriptor = wgpu::hal::TextureDescriptor {
            label: Some("dmabuf_video_frame"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::NV12,
            usage: wgpu::wgt::TextureUses::RESOURCE,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: vec![],
        };
        // The drop callback fires when wgpu destroys the texture, after all
        // submitted GPU work using it completed: destroy the image and let
        // go of the producer's buffer. The imported VkDeviceMemory is freed
        // by wgpu itself (TextureMemory::Dedicated).
        let owner = frame.owner.clone();
        let drop_device = raw_device.clone();
        // SAFETY: image/memory are valid, bound, and exclusively ours; the
        // callback keeps the image alive until wgpu is done with it.
        unsafe {
            hal_device.texture_from_raw(
                image,
                &hal_descriptor,
                Some(Box::new(move || {
                    // (Covered by the enclosing unsafe block:) the image is
                    // valid until this callback fires.
                    drop_device.destroy_image(image, None);
                    drop(owner);
                })),
                wgpu::hal::vulkan::TextureMemory::Dedicated(memory),
            )
        }
    };

    // SAFETY: the hal texture was created on this device with a matching
    // descriptor.
    let texture = unsafe {
        device.create_texture_from_hal::<Vulkan>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("dmabuf_video_frame"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::NV12,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
        )
    };
    Ok(texture)
}
