
package android.view;
public class View {
	public int getId() { return 0; }
	public String getIdName() { return null; }
	public int getLayoutDirection() { return 0; }
	public void setLayoutParams(ViewGroup.LayoutParams params) {}
	public void setId(int id) {}
	protected void onFinishInflate() {}
	public final boolean requestFocus() { return true; }
}
