package android.webkit;

import android.graphics.Bitmap;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

public final class WebViewCallbacksProbe {
  private static final String DEFAULT_USER_AGENT_MARKER = "Eclipse-WebView/152.0.6";

  private static final List<String> calls = new ArrayList<String>();

  private static final class RecordingClient extends WebViewClient {
    @Override
    public void onPageStarted(WebView view, String url, Bitmap favicon) {
      calls.add("started " + url + " " + favicon);
    }

    @Override
    public void onPageCommitVisible(WebView view, String url) {
      calls.add("committed " + url);
    }

    @Override
    public void onPageFinished(WebView view, String url) {
      calls.add("finished " + url);
    }

    @Override
    public void onLoadResource(WebView view, String url) {
      calls.add("resource " + url);
    }

    @Override
    public void onReceivedError(
        WebView view, int errorCode, String description, String failingUrl) {
      calls.add("error " + errorCode + " " + description + " " + failingUrl);
    }

    @Override
    public boolean shouldOverrideUrlLoading(WebView view, String url) {
      calls.add("override " + url);
      return true;
    }
  }

  private static final class RequestClient extends WebViewClient {
    @Override
    public boolean shouldOverrideUrlLoading(WebView view, WebResourceRequest request) {
      calls.add("request " + request.getUrl() + " " + request.getMethod() + " "
          + request.isRedirect() + " " + request.hasGesture() + " " + request.isForMainFrame()
          + " " + request.getRequestHeaders().isEmpty());
      return false;
    }
  }

  private static final class RecordingChromeClient extends WebChromeClient {
    @Override
    public void onProgressChanged(WebView view, int newProgress) {
      calls.add("progress " + newProgress);
    }
  }

  private interface NativeCall {
    void run();
  }

  private static void require(boolean condition, String message) {
    if (!condition) {
      throw new AssertionError(message);
    }
  }

  private static void requireCalls(String what, String... expected) {
    require(calls.equals(Arrays.asList(expected)), what + ": got " + calls);
    calls.clear();
  }

  private static void requireNative(String signature, NativeCall call) {
    try {
      call.run();
    } catch (UnsatisfiedLinkError expected) {
      String message = String.valueOf(expected.getMessage());
      require(message.contains(signature), "expected " + signature + ", got " + message);
      return;
    }
    throw new AssertionError(signature + " did not reach its native");
  }

  private static WebView newWebView(long widget) throws ReflectiveOperationException {
    Class<?> unsafeClass = Class.forName("sun.misc.Unsafe");
    Field theUnsafe = unsafeClass.getDeclaredField("theUnsafe");
    theUnsafe.setAccessible(true);
    WebView view = (WebView) unsafeClass.getMethod("allocateInstance", Class.class)
        .invoke(theUnsafe.get(null), WebView.class);
    Field widgetField = Class.forName("android.view.View").getDeclaredField("widget");
    widgetField.setAccessible(true);
    widgetField.setLong(view, widget);
    return view;
  }

  private static WebResourceRequest request(
      String url, String method, boolean redirect, boolean gesture)
      throws ReflectiveOperationException {
    Constructor<?> constructor = Class.forName("android.webkit.EclipseWebResourceRequest")
        .getDeclaredConstructor(String.class, String.class, boolean.class, boolean.class);
    constructor.setAccessible(true);
    return (WebResourceRequest) constructor.newInstance(url, method, redirect, gesture);
  }

  private static WebResourceError error(int code, String description)
      throws ReflectiveOperationException {
    Constructor<WebResourceError> constructor =
        WebResourceError.class.getDeclaredConstructor(int.class, CharSequence.class);
    constructor.setAccessible(true);
    return constructor.newInstance(code, description);
  }

  private static void checkSettings(WebView view, WebView other) {
    final WebSettings settings = view.getSettings();
    require(settings == view.getSettings(), "a WebView keeps one WebSettings");
    require(settings.widget == 42L, "the settings carry their WebView's handle");
    require(other.getSettings() != settings && other.getSettings().widget == 43L,
        "each WebView has its own settings");
    require(settings.getUserAgentString().contains(DEFAULT_USER_AGENT_MARKER),
        "a fresh WebView reports the default User-Agent");
    final String signature =
        "android.webkit.WebSettings.native_setUserAgentString(long, java.lang.String)";
    requireNative(signature, new NativeCall() {
      @Override
      public void run() {
        settings.setUserAgentString("Roblox/1");
      }
    });
    require("Roblox/1".equals(settings.getUserAgentString()), "the app's User-Agent is kept");
    require(other.getSettings().getUserAgentString().contains(DEFAULT_USER_AGENT_MARKER),
        "another WebView keeps its own User-Agent");
    requireNative(signature, new NativeCall() {
      @Override
      public void run() {
        settings.setUserAgentString("");
      }
    });
    require(settings.getUserAgentString().contains(DEFAULT_USER_AGENT_MARKER),
        "an empty User-Agent restores the default");
  }

  private static void checkCallbacks(WebView view) throws ReflectiveOperationException {
    view.internalLoadChanged(0, "https://a/");
    requireCalls("no client, no callback");
    view.setWebViewClient(new RecordingClient());
    view.internalLoadChanged(0, "https://a/");
    view.internalLoadChanged(1, "https://a/");
    view.internalLoadChanged(2, "https://a/");
    view.internalLoadChanged(3, "https://a/");
    requireCalls("load states", "started https://a/ null", "committed https://a/",
        "finished https://a/");

    view.internalLoadResource("https://a/app.js");
    requireCalls("resource", "resource https://a/app.js");

    view.internalReceivedError(
        request("https://nx.invalid/", "GET", false, false), error(-2, "Error resolving"));
    requireCalls("the default onReceivedError reaches the legacy callback",
        "error -2 Error resolving https://nx.invalid/");

    WebResourceRequest navigation = request("roblox://placeId=1", "GET", true, true);
    require(view.internalShouldOverrideUrlLoading(navigation), "the app's verdict is returned");
    requireCalls("the default request overload reaches the URL overload",
        "override roblox://placeId=1");
    view.setWebViewClient(new RequestClient());
    require(!view.internalShouldOverrideUrlLoading(navigation), "the app's verdict is returned");
    requireCalls("request fields", "request roblox://placeId=1 GET true true true true");
    view.setWebViewClient(null);
    require(!view.internalShouldOverrideUrlLoading(navigation), "no client leaves it to WebKit");
    requireCalls("no client, no policy callback");

    view.internalProgressChanged(40);
    requireCalls("no chrome client, no progress");
    view.setWebChromeClient(new RecordingChromeClient());
    view.internalProgressChanged(40);
    requireCalls("progress", "progress 40");
  }

  private static void checkNatives(final WebView view) {
    requireNative("android.webkit.WebView.native_getUrl(long)", new NativeCall() {
      @Override
      public void run() {
        view.getUrl();
      }
    });
    requireNative("android.webkit.WebView.native_reload(long)", new NativeCall() {
      @Override
      public void run() {
        view.reload();
      }
    });
    requireNative("android.webkit.WebView.native_stopLoading(long)", new NativeCall() {
      @Override
      public void run() {
        view.stopLoading();
      }
    });
    requireNative("android.webkit.WebView.native_removeJavascriptInterface(long, java.lang.String)",
        new NativeCall() {
          @Override
          public void run() {
            view.removeJavascriptInterface("__globalRobloxAndroidBridge__");
          }
        });
    requireNative("android.webkit.WebView.native_destroy(long)", new NativeCall() {
      @Override
      public void run() {
        view.destroy();
      }
    });
  }

  public static void main(String[] arguments) throws ReflectiveOperationException {
    WebView view = newWebView(42L);
    checkSettings(view, newWebView(43L));
    checkCallbacks(view);
    checkNatives(view);
    System.out.println("webview-callbacks-ok");
    Runtime.getRuntime().halt(0);
  }
}
