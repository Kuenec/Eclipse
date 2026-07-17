// 2026-07-16: compile-only stub (NEVER dexed) — web-engine M6 EclipseWebViewClientProbe.
// Declares the AOSP 3-arg onPageStarted that the classes2 smali shadow adds (patch-framework.sh
// §"(2) WebViewClient: NEW shadow into classes2"); the vendored ATL WebViewClient.java declares
// ONLY the 2-arg form, so compiling the probe against it would make the @Override a compile error.
// The two artifacts are tied by the descriptor literal in §3's post-append grep and §4c's dex
// back-check — a source grep cannot see what javac emitted (the 2026-07-02 lesson).
package android.webkit;

import android.graphics.Bitmap;

public class WebViewClient {
    public void onPageStarted(WebView view, String url) {}
    public void onPageStarted(WebView view, String url, Bitmap favicon) {}
    public void onPageFinished(WebView view, String url) {}
}
