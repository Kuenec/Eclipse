
package android.view;
public class ViewGroup extends View {
	public static class LayoutParams {
		public void resolveLayoutDirection(int layoutDirection) {}
	}
	public LayoutParams generateLayoutParams(android.util.AttributeSet attrs) { return null; }
	protected LayoutParams generateDefaultLayoutParams() { return null; }
	public void addView(View child) {}
	public void addView(View child, LayoutParams params) {}
}
