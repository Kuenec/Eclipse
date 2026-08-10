
package android.view;
import android.content.Context;
public class ContextThemeWrapper extends Context {
	public ContextThemeWrapper(Context context, int themeResId) {}
	public Object getSystemService(String name) { return null; }
}
