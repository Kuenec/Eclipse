// 2026-06-11: compile-only stub (`this_application`/`getPackageName` match ATL's
// api-impl Context; NEVER dexed).
package android.content;

public abstract class Context {
	public static android.app.Application this_application;

	public String getPackageName() {
		return null;
	}
}
