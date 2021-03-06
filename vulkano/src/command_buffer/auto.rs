// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

use std::error;
use std::fmt;
use std::iter;
use std::mem;
use std::slice;
use std::sync::Arc;

use OomError;
use buffer::BufferAccess;
use buffer::TypedBufferAccess;
use command_buffer::CommandBuffer;
use command_buffer::CommandBufferExecError;
use command_buffer::DrawIndirectCommand;
use command_buffer::DynamicState;
use command_buffer::StateCacher;
use command_buffer::StateCacherOutcome;
use command_buffer::pool::CommandPoolBuilderAlloc;
use command_buffer::pool::standard::StandardCommandPoolAlloc;
use command_buffer::pool::standard::StandardCommandPoolBuilder;
use command_buffer::synced::SyncCommandBuffer;
use command_buffer::synced::SyncCommandBufferBuilder;
use command_buffer::synced::SyncCommandBufferBuilderError;
use command_buffer::sys::Flags;
use command_buffer::sys::Kind;
use command_buffer::sys::UnsafeCommandBuffer;
use command_buffer::sys::UnsafeCommandBufferBuilderBufferImageCopy;
use command_buffer::sys::UnsafeCommandBufferBuilderColorImageClear;
use command_buffer::sys::UnsafeCommandBufferBuilderImageAspect;
use command_buffer::validity::*;
use descriptor::descriptor_set::DescriptorSetsCollection;
use descriptor::pipeline_layout::PipelineLayoutAbstract;
use device::Device;
use device::DeviceOwned;
use device::Queue;
use format::ClearValue;
use framebuffer::FramebufferAbstract;
use framebuffer::RenderPassDescClearValues;
use framebuffer::SubpassContents;
use image::ImageAccess;
use image::ImageLayout;
use instance::QueueFamily;
use pipeline::ComputePipelineAbstract;
use pipeline::GraphicsPipelineAbstract;
use pipeline::input_assembly::Index;
use pipeline::vertex::VertexSource;
use sync::AccessCheckError;
use sync::AccessFlagBits;
use sync::GpuFuture;
use sync::PipelineStages;

///
///
/// Note that command buffers allocated from the default command pool (`Arc<StandardCommandPool>`)
/// don't implement the `Send` and `Sync` traits. If you use this pool, then the
/// `AutoCommandBufferBuilder` will not implement `Send` and `Sync` either. Once a command buffer
/// is built, however, it *does* implement `Send` and `Sync`.
///
pub struct AutoCommandBufferBuilder<P = StandardCommandPoolBuilder> {
    inner: SyncCommandBufferBuilder<P>,
    state_cacher: StateCacher,

    // Contains the number of subpasses remaining in the current render pass, or `None` if we're
    // outside a render pass. If this is `Some(0)`, the user must call `end_render_pass`. If this
    // is `Some(1)` or more, the user must call `next_subpass`.
    subpasses_remaining: Option<usize>,

    // True if we are a secondary command buffer.
    secondary_cb: bool,

    // True if we're in a subpass that only allows executing secondary command buffers. False if
    // we're in a subpass that only allows inline commands. Irrelevant if not in a subpass.
    subpass_secondary: bool,
}

impl AutoCommandBufferBuilder<StandardCommandPoolBuilder> {
    pub fn new(device: Arc<Device>, queue_family: QueueFamily)
               -> Result<AutoCommandBufferBuilder<StandardCommandPoolBuilder>, OomError> {
        unsafe {
            let pool = Device::standard_command_pool(&device, queue_family);
            let inner = SyncCommandBufferBuilder::new(&pool, Kind::primary(), Flags::None);
            let state_cacher = StateCacher::new();

            Ok(AutoCommandBufferBuilder {
                   inner: inner?,
                   state_cacher: state_cacher,
                   subpasses_remaining: None,
                   secondary_cb: false,
                   subpass_secondary: false,
               })
        }
    }
}

impl<P> AutoCommandBufferBuilder<P> {
    #[inline]
    fn ensure_outside_render_pass(&self) -> Result<(), AutoCommandBufferBuilderContextError> {
        if self.subpasses_remaining.is_none() {
            Ok(())
        } else {
            Err(AutoCommandBufferBuilderContextError::ForbiddenInsideRenderPass)
        }
    }

    #[inline]
    fn ensure_inside_render_pass(&self, secondary: bool)
                                 -> Result<(), AutoCommandBufferBuilderContextError>
    {
        if self.subpasses_remaining.is_some() {
            if self.subpass_secondary == secondary {
                Ok(())
            } else {
                Err(AutoCommandBufferBuilderContextError::WrongSubpassType)
            }
        } else {
            Err(AutoCommandBufferBuilderContextError::ForbiddenOutsideRenderPass)
        }
    }

    /// Builds the command buffer.
    #[inline]
    pub fn build(self) -> Result<AutoCommandBuffer<P::Alloc>, BuildError>
        where P: CommandPoolBuilderAlloc
    {
        if self.secondary_cb {
            return Err(AutoCommandBufferBuilderContextError::ForbiddenInSecondary.into());
        }

        self.ensure_outside_render_pass()?;
        Ok(AutoCommandBuffer { inner: self.inner.build()? })
    }

    /// Adds a command that enters a render pass.
    ///
    /// If `secondary` is true, then you will only be able to add secondary command buffers while
    /// you're inside the first subpass of the render pass. If `secondary` is false, you will only
    /// be able to add inline draw commands and not secondary command buffers.
    ///
    /// You must call this before you can add draw commands.
    #[inline]
    pub fn begin_render_pass<F, C>(mut self, framebuffer: F, secondary: bool, clear_values: C)
                                   -> Result<Self, BeginRenderPassError>
        where F: FramebufferAbstract + RenderPassDescClearValues<C> + Send + Sync + 'static
    {
        unsafe {
            if self.secondary_cb {
                return Err(AutoCommandBufferBuilderContextError::ForbiddenInSecondary.into());
            }

            self.ensure_outside_render_pass()?;

            let clear_values = framebuffer.convert_clear_values(clear_values);
            let clear_values = clear_values.collect::<Vec<_>>().into_iter(); // TODO: necessary for Send + Sync ; needs an API rework of convert_clear_values
            let contents = if secondary { SubpassContents::SecondaryCommandBuffers }
                           else { SubpassContents::Inline };
            let num_subpasses = framebuffer.num_subpasses();
            debug_assert_ne!(num_subpasses, 0);
            self.inner
                .begin_render_pass(framebuffer, contents, clear_values)?;
            self.subpasses_remaining = Some(num_subpasses - 1);
            self.subpass_secondary = secondary;
            Ok(self)
        }
    }

    /// Adds a command that clears all the layers and mipmap levels of a color image with a
    /// specific value.
    ///
    /// # Panic
    ///
    /// Panics if `color` is not a color value.
    ///
    pub fn clear_color_image<I>(self, image: I, color: ClearValue)
                                -> Result<Self, ClearColorImageError>
        where I: ImageAccess + Send + Sync + 'static,
    {
        let layers = image.dimensions().array_layers();
        let levels = image.mipmap_levels();

        self.clear_color_image_dimensions(image, 0, layers, 0, levels, color)
    }

    /// Adds a command that clears a color image with a specific value.
    ///
    /// # Panic
    ///
    /// - Panics if `color` is not a color value.
    ///
    pub fn clear_color_image_dimensions<I>(mut self, image: I, first_layer: u32, num_layers: u32,
                                           first_mipmap: u32, num_mipmaps: u32, color: ClearValue)
                                           -> Result<Self, ClearColorImageError>
        where I: ImageAccess + Send + Sync + 'static,
    {
        unsafe {
            self.ensure_outside_render_pass()?;
            check_clear_color_image(self.device(), &image, first_layer, num_layers,
                                    first_mipmap, num_mipmaps)?;

            match color {
                ClearValue::Float(_) | ClearValue::Int(_) | ClearValue::Uint(_) => {},
                _ => panic!("The clear color is not a color value"),
            };
    
            let region = UnsafeCommandBufferBuilderColorImageClear {
                base_mip_level: first_mipmap,
                level_count: num_mipmaps,
                base_array_layer: first_layer,
                layer_count: num_layers,
            };

            // TODO: let choose layout
            self.inner.clear_color_image(image, ImageLayout::TransferDstOptimal, color,
                                         iter::once(region))?;
            Ok(self)
        }
    }

    /// Adds a command that copies from a buffer to another.
    ///
    /// This command will copy from the source to the destination. If their size is not equal, then
    /// the amount of data copied is equal to the smallest of the two.
    #[inline]
    pub fn copy_buffer<S, D, T>(mut self, source: S, destination: D) -> Result<Self, CopyBufferError>
        where S: TypedBufferAccess<Content = T> + Send + Sync + 'static,
              D: TypedBufferAccess<Content = T> + Send + Sync + 'static,
              T: ?Sized,
    {
        unsafe {
            self.ensure_outside_render_pass()?;
            let infos = check_copy_buffer(self.device(), &source, &destination)?;
            self.inner.copy_buffer(source, destination, iter::once((0, 0, infos.copy_size)))?;
            Ok(self)
        }
    }

    /// Adds a command that copies from a buffer to an image.
    pub fn copy_buffer_to_image<S, D>(self, source: S, destination: D)
                                      -> Result<Self, CopyBufferToImageError>
        where S: BufferAccess + Send + Sync + 'static,
              D: ImageAccess + Send + Sync + 'static
    {
        self.ensure_outside_render_pass()?;

        let dims = destination.dimensions().width_height_depth();
        self.copy_buffer_to_image_dimensions(source, destination, [0, 0, 0], dims, 0, 1, 0)
    }

    /// Adds a command that copies from a buffer to an image.
    pub fn copy_buffer_to_image_dimensions<S, D>(
        mut self, source: S, destination: D, offset: [u32; 3], size: [u32; 3], first_layer: u32,
        num_layers: u32, mipmap: u32) -> Result<Self, CopyBufferToImageError>
        where S: BufferAccess + Send + Sync + 'static,
              D: ImageAccess + Send + Sync + 'static
    {
        unsafe {
            self.ensure_outside_render_pass()?;

            // TODO: check validity
            // TODO: hastily implemented

            let copy = UnsafeCommandBufferBuilderBufferImageCopy {
                buffer_offset: 0,
                buffer_row_length: 0,
                buffer_image_height: 0,
                image_aspect: if destination.has_color() {
                    UnsafeCommandBufferBuilderImageAspect {
                        color: true,
                        depth: false,
                        stencil: false,
                    }
                } else {
                    unimplemented!()
                },
                image_mip_level: mipmap,
                image_base_array_layer: first_layer,
                image_layer_count: num_layers,
                image_offset: [offset[0] as i32, offset[1] as i32, offset[2] as i32],
                image_extent: size,
            };

            let size = source.size();
            self.inner.copy_buffer_to_image(source, destination, ImageLayout::TransferDstOptimal,     // TODO: let choose layout
                                            iter::once(copy))?;
            Ok(self)
        }
    }

    #[inline]
    pub fn dispatch<Cp, S, Pc>(mut self, dimensions: [u32; 3], pipeline: Cp, sets: S, constants: Pc)
                               -> Result<Self, DispatchError>
        where Cp: ComputePipelineAbstract + Send + Sync + 'static + Clone, // TODO: meh for Clone
              S: DescriptorSetsCollection
    {
        unsafe {
            self.ensure_outside_render_pass()?;
            check_push_constants_validity(&pipeline, &constants)?;
            check_descriptor_sets_validity(&pipeline, &sets)?;
            check_dispatch(pipeline.device(), dimensions)?;

            if let StateCacherOutcome::NeedChange =
                self.state_cacher.bind_compute_pipeline(&pipeline)
            {
                self.inner.bind_pipeline_compute(pipeline.clone());
            }

            push_constants(&mut self.inner, pipeline.clone(), constants);
            descriptor_sets(&mut self.inner, false, pipeline.clone(), sets)?;

            self.inner.dispatch(dimensions);
            Ok(self)
        }
    }

    #[inline]
    pub fn draw<V, Gp, S, Pc>(mut self, pipeline: Gp, dynamic: DynamicState, vertices: V, sets: S,
                              constants: Pc) -> Result<Self, DrawError>
        where Gp: GraphicsPipelineAbstract + VertexSource<V> + Send + Sync + 'static + Clone, // TODO: meh for Clone
              S: DescriptorSetsCollection
    {
        unsafe {
            // TODO: must check that pipeline is compatible with render pass

            self.ensure_inside_render_pass(false)?;
            check_dynamic_state_validity(&pipeline, &dynamic)?;
            check_push_constants_validity(&pipeline, &constants)?;
            check_descriptor_sets_validity(&pipeline, &sets)?;
            let vb_infos = check_vertex_buffers(&pipeline, vertices)?;

            if let StateCacherOutcome::NeedChange =
                self.state_cacher.bind_graphics_pipeline(&pipeline)
            {
                self.inner.bind_pipeline_graphics(pipeline.clone());
            }

            let dynamic = self.state_cacher.dynamic_state(dynamic);

            push_constants(&mut self.inner, pipeline.clone(), constants);
            set_state(&mut self.inner, dynamic);
            descriptor_sets(&mut self.inner, true, pipeline.clone(), sets)?;
            vertex_buffers(&mut self.inner, vb_infos.vertex_buffers)?;

            self.inner
                .draw(vb_infos.vertex_count as u32, vb_infos.instance_count as u32, 0, 0);
            Ok(self)
        }
    }

    #[inline]
    pub fn draw_indexed<V, Gp, S, Pc, Ib, I>(
        mut self, pipeline: Gp, dynamic: DynamicState, vertices: V, index_buffer: Ib, sets: S,
        constants: Pc)
        -> Result<Self, DrawIndexedError>
        where Gp: GraphicsPipelineAbstract + VertexSource<V> + Send + Sync + 'static + Clone, // TODO: meh for Clone
              S: DescriptorSetsCollection,
              Ib: BufferAccess + TypedBufferAccess<Content = [I]> + Send + Sync + 'static,
              I: Index + 'static
    {
        unsafe {
            // TODO: must check that pipeline is compatible with render pass

            self.ensure_inside_render_pass(false)?;
            let ib_infos = check_index_buffer(self.device(), &index_buffer)?;
            check_dynamic_state_validity(&pipeline, &dynamic)?;
            check_push_constants_validity(&pipeline, &constants)?;
            check_descriptor_sets_validity(&pipeline, &sets)?;
            let vb_infos = check_vertex_buffers(&pipeline, vertices)?;

            if let StateCacherOutcome::NeedChange =
                self.state_cacher.bind_graphics_pipeline(&pipeline)
            {
                self.inner.bind_pipeline_graphics(pipeline.clone());
            }

            if let StateCacherOutcome::NeedChange =
                self.state_cacher.bind_index_buffer(&index_buffer, I::ty())
            {
                self.inner.bind_index_buffer(index_buffer, I::ty())?;
            }

            let dynamic = self.state_cacher.dynamic_state(dynamic);

            push_constants(&mut self.inner, pipeline.clone(), constants);
            set_state(&mut self.inner, dynamic);
            descriptor_sets(&mut self.inner, true, pipeline.clone(), sets)?;
            vertex_buffers(&mut self.inner, vb_infos.vertex_buffers)?;
            // TODO: how to handle an index out of range of the vertex buffers?

            self.inner.draw_indexed(ib_infos.num_indices as u32, 1, 0, 0, 0);
            Ok(self)
        }
    }

    #[inline]
    pub fn draw_indirect<V, Gp, S, Pc, Ib>(mut self, pipeline: Gp, dynamic: DynamicState,
                                           vertices: V, indirect_buffer: Ib, sets: S, constants: Pc)
                                           -> Result<Self, DrawIndirectError>
        where Gp: GraphicsPipelineAbstract + VertexSource<V> + Send + Sync + 'static + Clone, // TODO: meh for Clone
              S: DescriptorSetsCollection,
              Ib: BufferAccess
                      + TypedBufferAccess<Content = [DrawIndirectCommand]>
                      + Send
                      + Sync
                      + 'static
    {
        unsafe {
            // TODO: must check that pipeline is compatible with render pass

            self.ensure_inside_render_pass(false)?;
            check_dynamic_state_validity(&pipeline, &dynamic)?;
            check_push_constants_validity(&pipeline, &constants)?;
            check_descriptor_sets_validity(&pipeline, &sets)?;
            let vb_infos = check_vertex_buffers(&pipeline, vertices)?;

            let draw_count = indirect_buffer.len() as u32;

            if let StateCacherOutcome::NeedChange =
                self.state_cacher.bind_graphics_pipeline(&pipeline)
            {
                self.inner.bind_pipeline_graphics(pipeline.clone());
            }

            let dynamic = self.state_cacher.dynamic_state(dynamic);

            push_constants(&mut self.inner, pipeline.clone(), constants);
            set_state(&mut self.inner, dynamic);
            descriptor_sets(&mut self.inner, true, pipeline.clone(), sets)?;
            vertex_buffers(&mut self.inner, vb_infos.vertex_buffers)?;

            self.inner.draw_indirect(indirect_buffer,
                                     draw_count,
                                     mem::size_of::<DrawIndirectCommand>() as u32)?;
            Ok(self)
        }
    }

    /// Adds a command that ends the current render pass.
    ///
    /// This must be called after you went through all the subpasses and before you can build
    /// the command buffer or add further commands.
    #[inline]
    pub fn end_render_pass(mut self) -> Result<Self, AutoCommandBufferBuilderContextError> {
        unsafe {
            if self.secondary_cb {
                return Err(AutoCommandBufferBuilderContextError::ForbiddenInSecondary);
            }

            match self.subpasses_remaining {
                Some(0) => (),
                None => {
                    return Err(AutoCommandBufferBuilderContextError::ForbiddenOutsideRenderPass);
                },
                Some(_) => {
                    return Err(AutoCommandBufferBuilderContextError::NumSubpassesMismatch);
                },
            }

            self.inner.end_render_pass();
            self.subpasses_remaining = None;
            Ok(self)
        }
    }

    /// Adds a command that writes the content of a buffer.
    ///
    /// This function is similar to the `memset` function in C. The `data` parameter is a number
    /// that will be repeatidely written through the entire buffer.
    ///
    /// > **Note**: This function is technically safe because buffers can only contain integers or
    /// > floating point numbers, which are always valid whatever their memory representation is.
    /// > But unless your buffer actually contains only 32-bits integers, you are encouraged to use
    /// > this function only for zeroing the content of a buffer by passing `0` for the data.
    // TODO: not safe because of signalling NaNs
    #[inline]
    pub fn fill_buffer<B>(mut self, buffer: B, data: u32) -> Result<Self, FillBufferError>
        where B: BufferAccess + Send + Sync + 'static
    {
        unsafe {
            self.ensure_outside_render_pass()?;
            check_fill_buffer(self.device(), &buffer)?;
            self.inner.fill_buffer(buffer, data);
            Ok(self)
        }
    }

    /// Adds a command that jumps to the next subpass of the current render pass.
    #[inline]
    pub fn next_subpass(mut self, secondary: bool)
                        -> Result<Self, AutoCommandBufferBuilderContextError> {
        unsafe {
            if self.secondary_cb {
                return Err(AutoCommandBufferBuilderContextError::ForbiddenInSecondary);
            }

            match self.subpasses_remaining {
                None => {
                    return Err(AutoCommandBufferBuilderContextError::ForbiddenOutsideRenderPass)
                },
                Some(0) => {
                    return Err(AutoCommandBufferBuilderContextError::NumSubpassesMismatch);
                },
                Some(ref mut num) => {
                    *num -= 1;
                }
            };

            self.subpass_secondary = secondary;

            let contents = if secondary { SubpassContents::SecondaryCommandBuffers }
                           else { SubpassContents::Inline };
            self.inner.next_subpass(contents);
            Ok(self)
        }
    }

    /// Adds a command that writes data to a buffer.
    ///
    /// If `data` is larger than the buffer, only the part of `data` that fits is written. If the
    /// buffer is larger than `data`, only the start of the buffer is written.
    // TODO: allow unsized values
    #[inline]
    pub fn update_buffer<B, D>(mut self, buffer: B, data: D) -> Result<Self, UpdateBufferError>
        where B: TypedBufferAccess<Content = D> + Send + Sync + 'static,
              D: Send + Sync + 'static
    {
        unsafe {
            self.ensure_outside_render_pass()?;
            check_update_buffer(self.device(), &buffer, &data)?;

            let size_of_data = mem::size_of_val(&data);
            if buffer.size() > size_of_data {
                self.inner.update_buffer(buffer, data);
            } else {
                unimplemented!() // TODO:
                //self.inner.update_buffer(buffer.slice(0 .. size_of_data), data);
            }

            Ok(self)
        }
    }
}

unsafe impl<P> DeviceOwned for AutoCommandBufferBuilder<P> {
    #[inline]
    fn device(&self) -> &Arc<Device> {
        self.inner.device()
    }
}

// Shortcut function to set the push constants.
unsafe fn push_constants<P, Pl, Pc>(destination: &mut SyncCommandBufferBuilder<P>, pipeline: Pl,
                                    push_constants: Pc)
    where Pl: PipelineLayoutAbstract + Send + Sync + Clone + 'static
{
    for num_range in 0 .. pipeline.num_push_constants_ranges() {
        let range = match pipeline.push_constants_range(num_range) {
            Some(r) => r,
            None => continue,
        };

        debug_assert_eq!(range.offset % 4, 0);
        debug_assert_eq!(range.size % 4, 0);

        let data = slice::from_raw_parts((&push_constants as *const Pc as *const u8)
                                             .offset(range.offset as isize),
                                         range.size as usize);

        destination.push_constants::<_, [u8]>(pipeline.clone(),
                                       range.stages,
                                       range.offset as u32,
                                       range.size as u32,
                                       data);
    }
}

// Shortcut function to change the state of the pipeline.
unsafe fn set_state<P>(destination: &mut SyncCommandBufferBuilder<P>, dynamic: DynamicState) {
    if let Some(line_width) = dynamic.line_width {
        destination.set_line_width(line_width);
    }

    if let Some(ref viewports) = dynamic.viewports {
        destination.set_viewport(0, viewports.iter().cloned().collect::<Vec<_>>().into_iter()); // TODO: don't collect
    }

    if let Some(ref scissors) = dynamic.scissors {
        destination.set_scissor(0, scissors.iter().cloned().collect::<Vec<_>>().into_iter()); // TODO: don't collect
    }
}

// Shortcut function to bind vertex buffers.
unsafe fn vertex_buffers<P>(destination: &mut SyncCommandBufferBuilder<P>,
                            vertex_buffers: Vec<Box<BufferAccess + Send + Sync>>)
                            -> Result<(), SyncCommandBufferBuilderError>
{
    let mut binder = destination.bind_vertex_buffers();
    for vb in vertex_buffers {
        binder.add(vb);
    }
    binder.submit(0)?;
    Ok(())
}

unsafe fn descriptor_sets<P, Pl, S>(destination: &mut SyncCommandBufferBuilder<P>, gfx: bool,
                                    pipeline: Pl, sets: S)
                                    -> Result<(), SyncCommandBufferBuilderError>
    where Pl: PipelineLayoutAbstract + Send + Sync + Clone + 'static,
          S: DescriptorSetsCollection
{
    let mut sets_binder = destination.bind_descriptor_sets();

    for set in sets.into_vec() {
        sets_binder.add(set);
    }

    sets_binder.submit(gfx, pipeline.clone(), 0, iter::empty())?;
    Ok(())
}

pub struct AutoCommandBuffer<P = StandardCommandPoolAlloc> {
    inner: SyncCommandBuffer<P>,
}

unsafe impl<P> CommandBuffer for AutoCommandBuffer<P> {
    type PoolAlloc = P;

    #[inline]
    fn inner(&self) -> &UnsafeCommandBuffer<P> {
        self.inner.inner()
    }

    #[inline]
    fn prepare_submit(&self, future: &GpuFuture, queue: &Queue)
                      -> Result<(), CommandBufferExecError> {
        self.inner.prepare_submit(future, queue)
    }

    #[inline]
    fn check_buffer_access(
        &self, buffer: &BufferAccess, exclusive: bool, queue: &Queue)
        -> Result<Option<(PipelineStages, AccessFlagBits)>, AccessCheckError> {
        self.inner.check_buffer_access(buffer, exclusive, queue)
    }

    #[inline]
    fn check_image_access(&self, image: &ImageAccess, layout: ImageLayout, exclusive: bool,
                          queue: &Queue)
                          -> Result<Option<(PipelineStages, AccessFlagBits)>, AccessCheckError> {
        self.inner
            .check_image_access(image, layout, exclusive, queue)
    }
}

unsafe impl<P> DeviceOwned for AutoCommandBuffer<P> {
    #[inline]
    fn device(&self) -> &Arc<Device> {
        self.inner.device()
    }
}

macro_rules! err_gen {
    ($name:ident { $($err:ident),+ }) => (
        #[derive(Debug, Clone)]
        pub enum $name {
            $(
                $err($err),
            )+
        }

        impl error::Error for $name {
            #[inline]
            fn description(&self) -> &str {
                match *self {
                    $(
                        $name::$err(_) => {
                            concat!("a ", stringify!($err))
                        }
                    )+
                }
            }

            #[inline]
            fn cause(&self) -> Option<&error::Error> {
                match *self {
                    $(
                        $name::$err(ref err) => Some(err),
                    )+
                }
            }
        }

        impl fmt::Display for $name {
            #[inline]
            fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
                write!(fmt, "{}", error::Error::description(self))
            }
        }

        $(
            impl From<$err> for $name {
                #[inline]
                fn from(err: $err) -> $name {
                    $name::$err(err)
                }
            }
        )+
    );
}

err_gen!(BuildError {
    AutoCommandBufferBuilderContextError,
    OomError
});

err_gen!(BeginRenderPassError {
    AutoCommandBufferBuilderContextError,
    SyncCommandBufferBuilderError
});

err_gen!(ClearColorImageError {
    AutoCommandBufferBuilderContextError,
    CheckClearColorImageError,
    SyncCommandBufferBuilderError
});

err_gen!(CopyBufferError {
    AutoCommandBufferBuilderContextError,
    CheckCopyBufferError,
    SyncCommandBufferBuilderError
});

err_gen!(CopyBufferToImageError {
    AutoCommandBufferBuilderContextError,
    SyncCommandBufferBuilderError
});

err_gen!(FillBufferError {
    AutoCommandBufferBuilderContextError,
    CheckFillBufferError
});

err_gen!(DispatchError {
    AutoCommandBufferBuilderContextError,
    CheckPushConstantsValidityError,
    CheckDescriptorSetsValidityError,
    CheckDispatchError,
    SyncCommandBufferBuilderError
});

err_gen!(DrawError {
    AutoCommandBufferBuilderContextError,
    CheckDynamicStateValidityError,
    CheckPushConstantsValidityError,
    CheckDescriptorSetsValidityError,
    CheckVertexBufferError,
    SyncCommandBufferBuilderError
});

err_gen!(DrawIndexedError {
    AutoCommandBufferBuilderContextError,
    CheckDynamicStateValidityError,
    CheckPushConstantsValidityError,
    CheckDescriptorSetsValidityError,
    CheckVertexBufferError,
    CheckIndexBufferError,
    SyncCommandBufferBuilderError
});

err_gen!(DrawIndirectError {
    AutoCommandBufferBuilderContextError,
    CheckDynamicStateValidityError,
    CheckPushConstantsValidityError,
    CheckDescriptorSetsValidityError,
    CheckVertexBufferError,
    SyncCommandBufferBuilderError
});

err_gen!(UpdateBufferError {
    AutoCommandBufferBuilderContextError,
    CheckUpdateBufferError
});

#[derive(Debug, Copy, Clone)]
pub enum AutoCommandBufferBuilderContextError {
    /// Operation forbidden in a secondary command buffer.
    ForbiddenInSecondary,
    /// Operation forbidden inside of a render pass.
    ForbiddenInsideRenderPass,
    /// Operation forbidden outside of a render pass.
    ForbiddenOutsideRenderPass,
    /// Tried to end a render pass with subpasses remaining, or tried to go to next subpass with no
    /// subpass remaining.
    NumSubpassesMismatch,
    /// Tried to execute a secondary command buffer inside a subpass that only allows inline
    /// commands, or a draw command in a subpass that only allows secondary command buffers.
    WrongSubpassType,
}

impl error::Error for AutoCommandBufferBuilderContextError {
    #[inline]
    fn description(&self) -> &str {
        match *self {
            AutoCommandBufferBuilderContextError::ForbiddenInSecondary => {
                "operation forbidden in a secondary command buffer"
            },
            AutoCommandBufferBuilderContextError::ForbiddenInsideRenderPass => {
                "operation forbidden inside of a render pass"
            },
            AutoCommandBufferBuilderContextError::ForbiddenOutsideRenderPass => {
                "operation forbidden outside of a render pass"
            },
            AutoCommandBufferBuilderContextError::NumSubpassesMismatch => {
                "tried to end a render pass with subpasses remaining, or tried to go to next \
                 subpass with no subpass remaining"
            },
            AutoCommandBufferBuilderContextError::WrongSubpassType => {
                "tried to execute a secondary command buffer inside a subpass that only allows \
                 inline commands, or a draw command in a subpass that only allows secondary \
                 command buffers"
            },
        }
    }
}

impl fmt::Display for AutoCommandBufferBuilderContextError {
    #[inline]
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(fmt, "{}", error::Error::description(self))
    }
}
