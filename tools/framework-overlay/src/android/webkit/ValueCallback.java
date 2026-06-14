package android.webkit;

/* ECLIPSE PATCH 2026-06-14: android.webkit.ValueCallback is an INTERFACE in AOSP, but ATL's stock
 * api-impl declares it as an empty `public class ValueCallback {}`. Roblox's obfuscated cookie class
 * (`ql.b`) `implements ValueCallback`, so ART throws
 *   java.lang.IncompatibleClassChangeError: Class ql.b implements non-interface class
 *   android.webkit.ValueCallback
 * at com.roblox.universalapp.cookie.CookieProtocol.<init> — killing the cookie/auth path (login)
 * and, because that runs on the main Looper via AsyncTask.finish, wedging the main-looper pump and
 * freezing the winit event loop (so no host input is delivered). Restore the AOSP interface shape
 * (the single onReceiveValue(T) callback). first-dex-wins puts this in classes.dex, shadowing the
 * stock class in classes3.dex. */
public interface ValueCallback<T> {
    public void onReceiveValue(T value);
}
