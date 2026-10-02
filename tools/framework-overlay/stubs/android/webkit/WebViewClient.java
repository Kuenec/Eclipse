package android.webkit;

import android.graphics.Bitmap;

public class WebViewClient {
  public void onPageStarted(WebView view, String url) {}

  public void onPageStarted(WebView view, String url, Bitmap favicon) {}

  public void onPageCommitVisible(WebView view, String url) {}

  public void onPageFinished(WebView view, String url) {}

  public void onLoadResource(WebView view, String url) {}

  public void onReceivedError(
      WebView view, WebResourceRequest request, WebResourceError error) {}

  public void onReceivedError(
      WebView view, int errorCode, String description, String failingUrl) {}

  public boolean shouldOverrideUrlLoading(WebView view, String url) {
    return false;
  }

  public boolean shouldOverrideUrlLoading(WebView view, WebResourceRequest request) {
    return false;
  }
}
