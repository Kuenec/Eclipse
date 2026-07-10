package android.webkit;

/* ECLIPSE PATCH 2026-07-09 (web-engine plan M4): a tiny INERT test probe used ONLY by the hidden
 * `__webview-test` subcommand (src/main.rs::run_webview_test). It is the `@JavascriptInterface`
 * bridge target (echo records its arg and returns "echo:<arg>") AND a ValueCallback recorder (its
 * onReceiveValue stores into a static field the harness polls). It has no effect unless the harness
 * constructs it. Staged into classes.dex like the ValueCallback/JavascriptInterface overlay classes.
 * The raw ValueCallback implementation is deliberate (the harness reads the erased onReceiveValue).*/
@SuppressWarnings({"rawtypes", "unchecked"})
public class EclipseBridgeProbe implements ValueCallback {
    /** The last echo() argument (the bridge round-trip evidence). */
    public static volatile String last;
    /** The last onReceiveValue() argument (an evaluateJavascript String result or a cookie Boolean). */
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
