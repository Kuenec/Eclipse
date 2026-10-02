package android.webkit;

import android.net.Uri;
import java.util.Collections;
import java.util.Map;

final class EclipseWebResourceRequest implements WebResourceRequest {
  private final Uri url;

  private final String method;

  private final boolean redirect;

  private final boolean gesture;

  EclipseWebResourceRequest(String url, String method, boolean redirect, boolean gesture) {
    this.url = Uri.parse(url);
    this.method = method;
    this.redirect = redirect;
    this.gesture = gesture;
  }

  @Override
  public Uri getUrl() {
    return url;
  }

  @Override
  public boolean isForMainFrame() {
    return true;
  }

  @Override
  public boolean isRedirect() {
    return redirect;
  }

  @Override
  public boolean hasGesture() {
    return gesture;
  }

  @Override
  public String getMethod() {
    return method;
  }

  @Override
  public Map<String, String> getRequestHeaders() {
    return Collections.emptyMap();
  }
}
