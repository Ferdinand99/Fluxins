//! What the client hands to the app, and how the app hears that the cache let go.

use std::sync::Arc;

use crate::DecodedImage;

/// Turns a decoded image into the object the UI keeps, once, and hears when the
/// cache drops it.
///
/// For gpui the handle is an `Arc<RenderImage>`:
///
/// ```ignore
/// struct GpuiImages { dropped: std::sync::mpsc::Sender<Arc<RenderImage>> }
/// impl ImageSink for GpuiImages {
///     type Handle = Arc<RenderImage>;
///     fn make(&self, image: DecodedImage) -> (Self::Handle, u64) {
///         let bytes = image.byte_size() * 2; // our copy is gone; gpui's CPU copy and the atlas tile remain
///         (Arc::new(RenderImage::new(image.into_gpui_frames())), bytes)
///     }
///     fn evicted(&self, handle: Self::Handle) {
///         let _ = self.dropped.send(handle); // the UI thread calls `cx.drop_image(handle, None)`
///     }
/// }
/// ```
///
/// With this, the pixels exist once on the CPU side (inside the `RenderImage`) and the
/// memory budget counts what the app says it holds, not only what this crate held.
pub trait ImageSink: Send + Sync + 'static {
    /// What the UI holds for one image. Cloning it must be cheap (use an `Arc`).
    type Handle: Clone + Send + Sync + 'static;

    /// Called once for every decoded image, on a blocking thread. Returns the
    /// handle and the bytes it keeps alive, which is what the memory cache counts.
    /// Count the GPU copy too if the budget should cover it.
    fn make(&self, image: DecodedImage) -> (Self::Handle, u64);

    /// Called when the cache no longer holds `handle`: evicted for space, replaced,
    /// dropped by `clear_memory`, or never stored because it is larger than the whole
    /// cache. The UI may still show the image, so release the platform resources
    /// (for gpui `cx.drop_image`) when the handle is no longer in use. Never called
    /// with a lock held, so it may call back into the client.
    fn evicted(&self, _handle: Self::Handle) {}
}

/// A sink that keeps the decoded pixels as they are.
#[derive(Debug, Clone, Copy, Default)]
pub struct RawImages;

impl ImageSink for RawImages {
    type Handle = Arc<DecodedImage>;

    fn make(&self, image: DecodedImage) -> (Self::Handle, u64) {
        let bytes = image.byte_size();
        (Arc::new(image), bytes)
    }
}
