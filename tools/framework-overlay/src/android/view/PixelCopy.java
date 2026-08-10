
package android.view;

import android.graphics.Bitmap;
import android.os.Handler;

public final class PixelCopy {
    public static final int SUCCESS = 0;
    public static final int ERROR_UNKNOWN = 1;
    public static final int ERROR_TIMEOUT = 2;
    public static final int ERROR_SOURCE_NO_DATA = 3;
    public static final int ERROR_SOURCE_INVALID = 4;
    public static final int ERROR_DESTINATION_INVALID = 5;

    public interface OnPixelCopyFinishedListener {
        void onPixelCopyFinished(int copyResult);
    }

    private PixelCopy() {}

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
