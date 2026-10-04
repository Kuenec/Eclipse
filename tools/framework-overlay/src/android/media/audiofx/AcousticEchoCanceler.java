package android.media.audiofx;

public class AcousticEchoCanceler extends AudioEffect {
  private AcousticEchoCanceler() {}

  public static boolean isAvailable() {
    return isEffectTypeAvailable(EFFECT_TYPE_AEC);
  }

  public static AcousticEchoCanceler create(int audioSession) {
    return null;
  }
}
