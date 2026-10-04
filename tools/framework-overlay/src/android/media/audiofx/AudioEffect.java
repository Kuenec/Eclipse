package android.media.audiofx;

import java.util.UUID;

public class AudioEffect {
  public static final UUID EFFECT_TYPE_AEC =
      UUID.fromString("7b491460-8d4d-11e0-bd61-0002a5d5c51b");
  public static final UUID EFFECT_TYPE_NS =
      UUID.fromString("58b4b260-8e06-11e0-aa8e-0002a5d5c51b");
  public static final int SUCCESS = 0;

  public static class Descriptor {
    public UUID type;
    public UUID uuid;
  }

  public static Descriptor[] queryEffects() {
    return new Descriptor[0];
  }

  public static boolean isEffectTypeAvailable(UUID type) {
    for (Descriptor descriptor : queryEffects()) {
      if (descriptor.type.equals(type)) {
        return true;
      }
    }
    return false;
  }

  public boolean getEnabled() {
    return false;
  }

  public int setEnabled(boolean enabled) {
    return SUCCESS;
  }

  public void release() {}
}
