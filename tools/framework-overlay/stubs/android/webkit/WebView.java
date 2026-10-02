package android.webkit;

public class WebView {
  public WebSettings getSettings() {
    return null;
  }

  public void setWebViewClient(WebViewClient client) {}

  public void setWebChromeClient(WebChromeClient client) {}

  public String getUrl() {
    return null;
  }

  public void reload() {}

  public void stopLoading() {}

  public void destroy() {}

  public void removeJavascriptInterface(String name) {}

  void internalLoadChanged(int loadState, String url) {}

  void internalProgressChanged(int progress) {}

  void internalLoadResource(String url) {}

  void internalReceivedError(WebResourceRequest request, WebResourceError error) {}

  boolean internalShouldOverrideUrlLoading(WebResourceRequest request) {
    return false;
  }
}
