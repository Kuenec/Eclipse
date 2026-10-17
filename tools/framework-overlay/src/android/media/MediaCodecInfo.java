package android.media;

import android.util.Range;

public final class MediaCodecInfo {
  private static final int MAX_SUPPORTED_INSTANCES = 16;
  private static final int MAX_SIZE = 4096;

  private final String name;
  private final String mime;

  MediaCodecInfo(String name, String mime) {
    this.name = name;
    this.mime = mime;
  }

  public String getName() {
    return name;
  }

  public boolean isEncoder() {
    return false;
  }

  public String[] getSupportedTypes() {
    return new String[] {mime};
  }

  public CodecCapabilities getCapabilitiesForType(String type) {
    if (!mime.equals(type)) {
      throw new IllegalArgumentException("codec " + name + " does not support type " + type);
    }
    return new CodecCapabilities(mime);
  }

  public static final class CodecCapabilities {
    public static final int COLOR_FormatYUV420SemiPlanar = 21;
    public static final int COLOR_FormatYUV420Flexible = 0x7F420888;

    public CodecProfileLevel[] profileLevels;
    public int[] colorFormats;

    private final VideoCapabilities videoCapabilities;
    private final AudioCapabilities audioCapabilities;

    CodecCapabilities(String mime) {
      profileLevels = profileLevelsFor(mime);
      if (mime.startsWith("video/")) {
        colorFormats = new int[] {COLOR_FormatYUV420Flexible, COLOR_FormatYUV420SemiPlanar};
        videoCapabilities = new VideoCapabilities();
        audioCapabilities = null;
      } else {
        colorFormats = new int[0];
        videoCapabilities = null;
        audioCapabilities = new AudioCapabilities();
      }
    }

    public int getMaxSupportedInstances() {
      return MAX_SUPPORTED_INSTANCES;
    }

    public VideoCapabilities getVideoCapabilities() {
      return videoCapabilities;
    }

    public AudioCapabilities getAudioCapabilities() {
      return audioCapabilities;
    }

    private static CodecProfileLevel[] profileLevelsFor(String mime) {
      switch (mime) {
        case "video/avc":
          return new CodecProfileLevel[] {
            new CodecProfileLevel(
                CodecProfileLevel.AVCProfileBaseline, CodecProfileLevel.AVCLevel52),
            new CodecProfileLevel(CodecProfileLevel.AVCProfileMain, CodecProfileLevel.AVCLevel52),
            new CodecProfileLevel(CodecProfileLevel.AVCProfileHigh, CodecProfileLevel.AVCLevel52),
          };
        case "video/hevc":
          return new CodecProfileLevel[] {
            new CodecProfileLevel(
                CodecProfileLevel.HEVCProfileMain, CodecProfileLevel.HEVCMainTierLevel51),
          };
        case "audio/mp4a-latm":
          return new CodecProfileLevel[] {
            new CodecProfileLevel(CodecProfileLevel.AACObjectLC, 0),
            new CodecProfileLevel(CodecProfileLevel.AACObjectHE, 0),
            new CodecProfileLevel(CodecProfileLevel.AACObjectHE_PS, 0),
          };
        default:
          return new CodecProfileLevel[0];
      }
    }
  }

  public static final class VideoCapabilities {
    private static final int ALIGNMENT = 2;
    private static final Range<Integer> SIZES = Range.create(ALIGNMENT, MAX_SIZE);
    private static final Range<Integer> FRAME_RATES = Range.create(1, 960);
    private static final Range<Integer> BITRATES = Range.create(1, 240000000);

    VideoCapabilities() {}

    public Range<Integer> getSupportedWidths() {
      return SIZES;
    }

    public Range<Integer> getSupportedHeights() {
      return SIZES;
    }

    public int getWidthAlignment() {
      return ALIGNMENT;
    }

    public int getHeightAlignment() {
      return ALIGNMENT;
    }

    public Range<Integer> getSupportedFrameRates() {
      return FRAME_RATES;
    }

    public Range<Integer> getBitrateRange() {
      return BITRATES;
    }

    public boolean isSizeSupported(int width, int height) {
      return SIZES.contains(width)
          && SIZES.contains(height)
          && width % ALIGNMENT == 0
          && height % ALIGNMENT == 0;
    }

    public boolean areSizeAndRateSupported(int width, int height, double frameRate) {
      return isSizeSupported(width, height)
          && frameRate >= FRAME_RATES.getLower()
          && frameRate <= FRAME_RATES.getUpper();
    }
  }

  public static final class AudioCapabilities {
    AudioCapabilities() {}
  }

  public static final class CodecProfileLevel {
    public static final int AVCProfileBaseline = 0x01;
    public static final int AVCProfileMain = 0x02;
    public static final int AVCProfileExtended = 0x04;
    public static final int AVCProfileHigh = 0x08;
    public static final int AVCProfileHigh10 = 0x10;
    public static final int AVCProfileHigh422 = 0x20;
    public static final int AVCProfileHigh444 = 0x40;
    public static final int AVCProfileConstrainedBaseline = 0x10000;
    public static final int AVCProfileConstrainedHigh = 0x80000;

    public static final int AVCLevel1 = 0x01;
    public static final int AVCLevel1b = 0x02;
    public static final int AVCLevel11 = 0x04;
    public static final int AVCLevel12 = 0x08;
    public static final int AVCLevel13 = 0x10;
    public static final int AVCLevel2 = 0x20;
    public static final int AVCLevel21 = 0x40;
    public static final int AVCLevel22 = 0x80;
    public static final int AVCLevel3 = 0x100;
    public static final int AVCLevel31 = 0x200;
    public static final int AVCLevel32 = 0x400;
    public static final int AVCLevel4 = 0x800;
    public static final int AVCLevel41 = 0x1000;
    public static final int AVCLevel42 = 0x2000;
    public static final int AVCLevel5 = 0x4000;
    public static final int AVCLevel51 = 0x8000;
    public static final int AVCLevel52 = 0x10000;
    public static final int AVCLevel6 = 0x20000;
    public static final int AVCLevel61 = 0x40000;
    public static final int AVCLevel62 = 0x80000;

    public static final int HEVCProfileMain = 0x01;
    public static final int HEVCProfileMain10 = 0x02;
    public static final int HEVCProfileMainStill = 0x04;
    public static final int HEVCProfileMain10HDR10 = 0x1000;
    public static final int HEVCProfileMain10HDR10Plus = 0x2000;

    public static final int HEVCMainTierLevel1 = 0x1;
    public static final int HEVCHighTierLevel1 = 0x2;
    public static final int HEVCMainTierLevel2 = 0x4;
    public static final int HEVCHighTierLevel2 = 0x8;
    public static final int HEVCMainTierLevel21 = 0x10;
    public static final int HEVCHighTierLevel21 = 0x20;
    public static final int HEVCMainTierLevel3 = 0x40;
    public static final int HEVCHighTierLevel3 = 0x80;
    public static final int HEVCMainTierLevel31 = 0x100;
    public static final int HEVCHighTierLevel31 = 0x200;
    public static final int HEVCMainTierLevel4 = 0x400;
    public static final int HEVCHighTierLevel4 = 0x800;
    public static final int HEVCMainTierLevel41 = 0x1000;
    public static final int HEVCHighTierLevel41 = 0x2000;
    public static final int HEVCMainTierLevel5 = 0x4000;
    public static final int HEVCHighTierLevel5 = 0x8000;
    public static final int HEVCMainTierLevel51 = 0x10000;
    public static final int HEVCHighTierLevel51 = 0x20000;
    public static final int HEVCMainTierLevel52 = 0x40000;
    public static final int HEVCHighTierLevel52 = 0x80000;
    public static final int HEVCMainTierLevel6 = 0x100000;
    public static final int HEVCHighTierLevel6 = 0x200000;
    public static final int HEVCMainTierLevel61 = 0x400000;
    public static final int HEVCHighTierLevel61 = 0x800000;
    public static final int HEVCMainTierLevel62 = 0x1000000;
    public static final int HEVCHighTierLevel62 = 0x2000000;

    public static final int VP8ProfileMain = 0x01;

    public static final int VP8Level_Version0 = 0x01;
    public static final int VP8Level_Version1 = 0x02;
    public static final int VP8Level_Version2 = 0x04;
    public static final int VP8Level_Version3 = 0x08;

    public static final int VP9Profile0 = 0x01;
    public static final int VP9Profile1 = 0x02;
    public static final int VP9Profile2 = 0x04;
    public static final int VP9Profile3 = 0x08;
    public static final int VP9Profile2HDR = 0x1000;
    public static final int VP9Profile3HDR = 0x2000;
    public static final int VP9Profile2HDR10Plus = 0x4000;
    public static final int VP9Profile3HDR10Plus = 0x8000;

    public static final int VP9Level1 = 0x1;
    public static final int VP9Level11 = 0x2;
    public static final int VP9Level2 = 0x4;
    public static final int VP9Level21 = 0x8;
    public static final int VP9Level3 = 0x10;
    public static final int VP9Level31 = 0x20;
    public static final int VP9Level4 = 0x40;
    public static final int VP9Level41 = 0x80;
    public static final int VP9Level5 = 0x100;
    public static final int VP9Level51 = 0x200;
    public static final int VP9Level52 = 0x400;
    public static final int VP9Level6 = 0x800;
    public static final int VP9Level61 = 0x1000;
    public static final int VP9Level62 = 0x2000;

    public static final int AACObjectMain = 1;
    public static final int AACObjectLC = 2;
    public static final int AACObjectSSR = 3;
    public static final int AACObjectLTP = 4;
    public static final int AACObjectHE = 5;
    public static final int AACObjectScalable = 6;
    public static final int AACObjectERLC = 17;
    public static final int AACObjectERScalable = 20;
    public static final int AACObjectLD = 23;
    public static final int AACObjectHE_PS = 29;
    public static final int AACObjectELD = 39;
    public static final int AACObjectXHE = 42;

    public int profile;
    public int level;

    public CodecProfileLevel() {}

    CodecProfileLevel(int profile, int level) {
      this.profile = profile;
      this.level = level;
    }
  }
}
