package android.util;

public final class Range<T extends Comparable<? super T>> {
  private final T lower;
  private final T upper;

  public Range(T lower, T upper) {
    if (lower == null) {
      throw new NullPointerException("lower must not be null");
    }
    if (upper == null) {
      throw new NullPointerException("upper must not be null");
    }
    if (lower.compareTo(upper) > 0) {
      throw new IllegalArgumentException("lower must be less than or equal to upper");
    }
    this.lower = lower;
    this.upper = upper;
  }

  public static <T extends Comparable<? super T>> Range<T> create(T lower, T upper) {
    return new Range<T>(lower, upper);
  }

  public T getLower() {
    return lower;
  }

  public T getUpper() {
    return upper;
  }

  public boolean contains(T value) {
    return value.compareTo(lower) >= 0 && value.compareTo(upper) <= 0;
  }

  public T clamp(T value) {
    if (value.compareTo(lower) < 0) {
      return lower;
    }
    if (value.compareTo(upper) > 0) {
      return upper;
    }
    return value;
  }

  public Range<T> intersect(Range<T> range) {
    T higherLower = range.lower.compareTo(lower) > 0 ? range.lower : lower;
    T lowerUpper = range.upper.compareTo(upper) < 0 ? range.upper : upper;
    return create(higherLower, lowerUpper);
  }

  public Range<T> extend(Range<T> range) {
    T lowerLower = range.lower.compareTo(lower) < 0 ? range.lower : lower;
    T higherUpper = range.upper.compareTo(upper) > 0 ? range.upper : upper;
    return create(lowerLower, higherUpper);
  }

  @Override
  public boolean equals(Object other) {
    if (this == other) {
      return true;
    }
    if (!(other instanceof Range<?>)) {
      return false;
    }
    Range<?> range = (Range<?>) other;
    return lower.equals(range.lower) && upper.equals(range.upper);
  }

  @Override
  public int hashCode() {
    return 31 * lower.hashCode() + upper.hashCode();
  }

  @Override
  public String toString() {
    return "[" + lower + ", " + upper + "]";
  }
}
