// 2026-06-11: compile-only stub (NEVER dexed).
package android.os;

public interface Parcelable {
	int describeContents();

	void writeToParcel(Parcel dest, int flags);
}
