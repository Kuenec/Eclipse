// 2026-07-17: Eclipse compatibility surface for Android's API-24 PixelCopy contract.
package android.view;

import android.graphics.Bitmap;
import android.os.Handler;

/**
 * Reports an honest asynchronous failure when an app asks Eclipse's headless Android surface to
 * perform a framework pixel copy.
 *
 * <p>Eclipse does not own an Android SurfaceFlinger/ThreadedRenderer pixel-copy backend. Returning
 * {@link #SUCCESS} would therefore fabricate pixels. Android defines {@link #ERROR_SOURCE_NO_DATA}
 * for a source with no queued buffer; that is the exact state of the framework-side SurfaceView.
 */
public final class PixelCopy {
    public static final int SUCCESS = 0;
    public static final int ERROR_UNKNOWN = 1;
    public static final int ERROR_TIMEOUT = 2;
    public static final int ERROR_SOURCE_NO_DATA = 3;
    public static final int ERROR_SOURCE_INVALID = 4;
    public static final int ERROR_DESTINATION_INVALID = 5;

    /** Callback invoked on the supplied Handler after a request completes. */
    public interface OnPixelCopyFinishedListener {
        void onPixelCopyFinished(int copyResult);
    }

    private PixelCopy() {}

    /**
     * Implements the API-24 SurfaceView overload used by Roblox's transition screenshot path.
     */
    public static void request(
            SurfaceView source,
            Bitmap dest,
            final OnPixelCopyFinishedListener listener,
            Handler listenerThread) {
        if (source == null) {
            throw new IllegalArgumentException("SurfaceView cannot be null");
        }
        validateBitmapDest(dest);
        if (listener == null) {
            throw new IllegalArgumentException("Listener cannot be null");
        }
        if (listenerThread == null) {
            throw new IllegalArgumentException("Handler cannot be null");
        }

        // Android's implementation posts completion to this Handler and intentionally ignores
        // Handler.post's boolean result. Preserve that dispatch contract; only the result differs
        // because Eclipse has no framework-side pixel buffer to copy.
        listenerThread.post(new Runnable() {
            @Override
            public void run() {
                listener.onPixelCopyFinished(ERROR_SOURCE_NO_DATA);
            }
        });
    }

    private static void validateBitmapDest(Bitmap bitmap) {
        if (bitmap == null) {
            throw new IllegalArgumentException("Bitmap cannot be null");
        }
        if (bitmap.isRecycled()) {
            throw new IllegalArgumentException("Bitmap is recycled");
        }
        if (!bitmap.isMutable()) {
            throw new IllegalArgumentException("Bitmap is immutable");
        }
    }
}
