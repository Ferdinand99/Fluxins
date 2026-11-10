//! What the client hands to the app, and how the app hears that the cache let go.

use std::sync::Arc;

use crate::DecodedImage;

/// Turns a decoded image into the object the UI keeps, once, and hears when the cache
/// lets go of it.
///
/// For gpui the handle is an `Arc<RenderImage>`. **`evicted` must not drop the
/// texture at once.** The cache letting go of an image does not mean the screen has:
/// a row that is still visible holds the same `Arc` and paints it every frame. If
/// `cx.drop_image` ran now, gpui would remove the texture from its atlas and upload
/// it again on the next paint, and that second tile would stay until the window
/// closes. Queue the handle, and drop it only when nothing paints it any more:
///
/// ```ignore
/// struct GpuiImages { dropped: std::sync::mpsc::Sender<Arc<RenderImage>> }
/// impl ImageSink for GpuiImages {
///     type Handle = Arc<RenderImage>;
///     fn make(&self, image: DecodedImage) -> (Self::Handle, u64) {
///         let bytes = image.byte_size() * 2; // gpui's CPU copy plus the atlas tile
///         (Arc::new(RenderImage::new(image.into_gpui_frames())), bytes)
///     }
///     fn evicted(&self, handle: Self::Handle) {
///         let _ = self.dropped.send(handle); // only queue it; this is not the UI thread
///     }
/// }
///
/// // On the UI thread, once per frame, after painting:
/// fn release_unused(pending: &mut Vec<Arc<RenderImage>>, rx: &Receiver<Arc<RenderImage>>, cx: &mut App) {
///     pending.extend(rx.try_iter());
///     pending.retain(|image| {
///         // The queue holds one reference. Any more means a view still holds the
///         // image (and paints it), so wait for a later frame.
///         if Arc::strong_count(image) > 1 {
///             return true;
///         }
///         cx.drop_image(image.clone(), None);
///         false
///     });
/// }
/// ```
///
/// Views must hold the `Arc` they paint (not look it up in the cache each frame), so
/// that the count says whether anything still shows the image. The other pattern
/// that works: record in each frame which images were painted, and drop an evicted
/// one only when it was not painted this frame.
///
/// With this, the pixels exist once on the CPU side (inside the `RenderImage`) and
/// the memory budget counts what the app says it holds, not only what this crate held.
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
    /// (for gpui `cx.drop_image`) only when the handle is no longer in use; see the
    /// pattern above. Never called with a lock held, so it may call back into the
    /// client. It runs on a runtime thread before the waiters of the `load` that caused
    /// it are woken, so **it must not block** (no full bounded channel to the UI
    /// thread, no lock that the UI holds): queue the handle and return. A panic in it
    /// is caught and logged, and the request still completes.
    /// For evictions that a `load` causes, it has been called by the time that
    /// `load` returns.
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
