package android.webkit;

public class EclipseBridgeProbe implements ValueCallback<Object> {
  public static volatile String last;

  public static volatile Object lastValue;

  @JavascriptInterface
  public String echo(String s) {
    last = s;
    return "echo:" + s;
  }

  @Override
  public void onReceiveValue(Object value) {
    lastValue = value;
  }
}
