package android.webkit;

import android.graphics.Bitmap;
import android.os.Handler;
import android.os.Looper;

public class EclipseWebViewClientProbe extends WebViewClient {
  public static volatile Handler lastHandler;

  @Override
  public void onPageStarted(WebView view, String url, Bitmap favicon) {
    assertUiThread("onPageStarted");
    lastHandler = new Handler();
  }

  @Override
  public void onPageFinished(WebView view, String url) {
    assertUiThread("onPageFinished");
    lastHandler = new Handler();
  }

  private static void assertUiThread(String cb) {
    if (Looper.myLooper() != Looper.getMainLooper()) {
      throw new IllegalStateException("ECLIPSE: WebViewClient." + cb
          + " was not dispatched on the UI thread (AOSP "
          + "delivers it there) — thread=" + Thread.currentThread().getName());
    }
  }
}
