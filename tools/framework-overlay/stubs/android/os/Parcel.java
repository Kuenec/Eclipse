// 2026-06-11: compile-only stub (NEVER dexed).
// 2026-06-13: + writeLong/writeInt so MemoryInfo.writeToParcel compiles; the real (stock) Parcel
// provides them at runtime (verified present on ATL's installed android.os.Parcel).
package android.os;

public class Parcel {
	public void writeLong(long value) {}

	public void writeInt(int value) {}
}
