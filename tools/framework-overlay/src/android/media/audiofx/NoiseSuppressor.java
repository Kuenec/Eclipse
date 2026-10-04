package android.media.audiofx;

public class NoiseSuppressor extends AudioEffect {
  private NoiseSuppressor() {}

  public static boolean isAvailable() {
    return isEffectTypeAvailable(EFFECT_TYPE_NS);
  }

  public static NoiseSuppressor create(int audioSession) {
    return null;
  }
}
