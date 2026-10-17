package android.media;

public final class MediaCodecList {
  private static MediaCodecInfo[] infos;

  public MediaCodecList(int kind) {}

  public static int getCodecCount() {
    return infos().length;
  }

  public static MediaCodecInfo getCodecInfoAt(int index) {
    MediaCodecInfo[] decoders = infos();
    if (index < 0 || index >= decoders.length) {
      throw new IllegalArgumentException("no codec at index " + index);
    }
    return decoders[index];
  }

  public MediaCodecInfo[] getCodecInfos() {
    return infos().clone();
  }

  static MediaCodecInfo[] infosFor(String[] decoders) {
    if (decoders.length % 2 != 0) {
      throw new IllegalArgumentException("each decoder name needs a MIME type");
    }
    MediaCodecInfo[] list = new MediaCodecInfo[decoders.length / 2];
    for (int i = 0; i < list.length; i++) {
      list[i] = new MediaCodecInfo(decoders[2 * i], decoders[2 * i + 1]);
    }
    return list;
  }

  private static synchronized MediaCodecInfo[] infos() {
    if (infos == null) {
      infos = infosFor(native_decoders());
    }
    return infos;
  }

  private static native String[] native_decoders();
}
