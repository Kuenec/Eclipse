package android.webkit;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/* ECLIPSE PATCH 2026-07-09 (web-engine plan M4): android.webkit.JavascriptInterface is the AOSP
 * (API 17+) RUNTIME-retention annotation that marks the ONLY methods addJavascriptInterface exposes
 * to page JS. ATL's stock/vendored framework OMITS it, so the reflective @JavascriptInterface
 * filtering (framework.rs::reflect_javascript_interface_methods) and the real app's own bridge
 * methods NoClassDefFound without it. Staged into classes.dex (first-dex-wins, like ValueCallback). */
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
public @interface JavascriptInterface {}
