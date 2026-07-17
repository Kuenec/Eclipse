package android.webkit;

import android.graphics.Bitmap;
import android.os.Handler;
import android.os.Looper;

/* ECLIPSE PROBE 2026-07-16 (web-engine plan M6): `__webview-test`'s driven WebViewClient — the
 * STRUCTURAL guard for "app-facing WebView callbacks are delivered on the UI (Looper) thread".
 * Inert: it posts nothing and touches no engine state. It carries the ONE thing the stock
 * `new WebViewClient()` harness could not see — a real app's page callbacks CONSTRUCT AN
 * android.os.Handler. Roblox's do, via SwipeRefreshLayout.setRefreshing -> View.startAnimation ->
 * Animation.start -> new Handler(). The no-arg Handler.<init> reads Looper.myLooper() and throws
 * "Can't create handler inside thread that has not called Looper.prepare()" on a Looper-less
 * dispatch thread (ATL Handler.java:197), so this probe fails the harness exactly where the
 * 2026-07-16 live boots failed. The UI-thread check pins the OTHER half of the AOSP contract:
 * without it, "just Looper.prepare() on the upcall thread" — the tempting WRONG fix, which makes
 * new Handler() succeed and then silently swallows every post — would pass this guard green.
 * Overriding the AOSP 3-arg onPageStarted (not ATL's 2-arg) also pins the M6 3-arg dispatch: an
 * AOSP-compiled app overrides exactly this form. Staged into classes.dex like the
 * EclipseBridgeProbe / ValueCallback overlay classes; its superclass resolves from the
 * classes2 smali shadow (first-dex-wins), which is where the 3-arg base lives. */
public class EclipseWebViewClientProbe extends WebViewClient {
    /** Held in a static so nothing can elide the construction (the app holds its Handler too). */
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
            throw new IllegalStateException(
                "ECLIPSE: WebViewClient." + cb + " was not dispatched on the UI thread (AOSP "
                + "delivers it there) — thread=" + Thread.currentThread().getName());
        }
    }
}
