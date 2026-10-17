package android.media;

import android.util.Range;
import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;

public final class MediaCodecListProbe {
  private static final String[] WITH_CODECS_EXTRA = {
    "aac", "audio/mp4a-latm",
    "h264", "video/avc",
    "hevc", "video/hevc",
    "mp3", "audio/mpeg",
    "opus", "audio/opus",
  };
  private static final String[] WITHOUT_CODECS_EXTRA = {
    "aac", "audio/mp4a-latm",
    "mp3", "audio/mpeg",
    "opus", "audio/opus",
  };
  private static final String AVC_CAPABILITY =
      "h264 AVC decoder hardware profiles=[AVCProfileBaseline, AVCProfileMain, AVCProfileHigh]"
          + " levels=[AVCLevel52, AVCLevel52, AVCLevel52] instances=16 width=2-4096"
          + " height=2-4096 fps=1-960 bitrate=1-240000000";
  private static final String HEVC_CAPABILITY =
      "hevc HEVC decoder hardware profiles=[HEVCProfileMain] levels=[HEVCMainTierLevel51]"
          + " instances=16 width=2-4096 height=2-4096 fps=1-960 bitrate=1-240000000";

  private static final Map<Integer, List<String>> PROFILES = namesByValue("Profile");
  private static final Map<Integer, List<String>> LEVELS = namesByValue("Level");
  private static final Map<Integer, List<String>> AAC_OBJECTS = namesByValue("AACObject");
  private static final Map<String, String> FAMILIES = new LinkedHashMap<String, String>();

  static {
    FAMILIES.put("heic", "HEVC");
    FAMILIES.put("hevc", "HEVC");
    FAMILIES.put("avc", "AVC");
    FAMILIES.put("3gpp", "H263");
    FAMILIES.put("mpeg2", "MPEG2");
    FAMILIES.put("mp4v-es", "MPEG4");
    FAMILIES.put("vp8", "VP8");
    FAMILIES.put("vp9", "VP9");
    FAMILIES.put("av01", "AV1");
    FAMILIES.put("vorbis", "Vorbis");
    FAMILIES.put("opus", "Opus");
  }

  private static void require(boolean condition, String message) {
    if (!condition) {
      throw new AssertionError(message);
    }
  }

  private static Map<Integer, List<String>> namesByValue(String part) {
    Class<?> holder = MediaCodecInfo.CodecProfileLevel.class;
    List<Field> fields = new ArrayList<Field>(Arrays.asList(holder.getFields()));
    fields.addAll(Arrays.asList(holder.getDeclaredFields()));
    Map<Integer, List<String>> names = new LinkedHashMap<Integer, List<String>>();
    for (Field field : fields) {
      if (!field.getName().contains(part)) {
        continue;
      }
      try {
        Integer value = field.getInt(null);
        if (!names.containsKey(value)) {
          names.put(value, new ArrayList<String>());
        }
        names.get(value).add(field.getName());
      } catch (IllegalAccessException ignored) {
        continue;
      }
    }
    return names;
  }

  private static String family(String type) {
    if (type.contains("aac") || type.contains("mp4a")) {
      return "AAC";
    }
    for (Map.Entry<String, String> entry : FAMILIES.entrySet()) {
      if (type.contains(entry.getKey())) {
        return entry.getValue();
      }
    }
    return null;
  }

  private static String pick(List<String> names, String family, int value) {
    for (String name : names) {
      if (family == null || name.contains(family)) {
        return name;
      }
    }
    return String.valueOf(value);
  }

  private static List<String> namesOf(Map<Integer, List<String>> names, int value) {
    List<String> found = names.get(value);
    return found == null ? Collections.<String>emptyList() : found;
  }

  private static String[] profileLevelNames(String type, MediaCodecInfo.CodecProfileLevel entry) {
    String family = family(type);
    boolean aac = "AAC".equals(family);
    String profile =
        pick(namesOf(aac ? AAC_OBJECTS : PROFILES, entry.profile), family, entry.profile);
    String level =
        aac
            ? String.valueOf(entry.level)
            : pick(namesOf(LEVELS, entry.level), family, entry.level);
    return new String[] {profile, level};
  }

  private static boolean isSoftware(MediaCodecInfo info) {
    String name = info.getName().toLowerCase(Locale.ROOT);
    return name.startsWith("omx.google.")
        || name.startsWith("c2.android.")
        || name.contains(".sw.dec")
        || name.contains(".sw.enc")
        || name.contains("ffmpeg");
  }

  private static boolean hasVideoType(MediaCodecInfo info) {
    for (String type : info.getSupportedTypes()) {
      if (info.getCapabilitiesForType(type).getVideoCapabilities() != null) {
        return true;
      }
    }
    return false;
  }

  private static String familyOf(MediaCodecInfo info) {
    for (String type : info.getSupportedTypes()) {
      String family = family(type);
      if (family != null) {
        return family;
      }
    }
    return null;
  }

  private static String span(Range<Integer> range) {
    return range.getLower().intValue() + "-" + range.getUpper().intValue();
  }

  private static String capability(MediaCodecInfo info, String type) {
    MediaCodecInfo.CodecCapabilities capabilities;
    try {
      capabilities = info.getCapabilitiesForType(type);
    } catch (Exception unsupported) {
      return null;
    }
    List<String> profiles = new ArrayList<String>();
    List<String> levels = new ArrayList<String>();
    for (MediaCodecInfo.CodecProfileLevel entry : capabilities.profileLevels) {
      String[] names = profileLevelNames(type, entry);
      profiles.add(names[0]);
      levels.add(names[1]);
    }
    MediaCodecInfo.VideoCapabilities video = capabilities.getVideoCapabilities();
    return info.getName()
        + " "
        + family(type)
        + (info.isEncoder() ? " encoder" : " decoder")
        + (isSoftware(info) ? " software" : " hardware")
        + " profiles="
        + profiles
        + " levels="
        + levels
        + " instances="
        + capabilities.getMaxSupportedInstances()
        + " width="
        + span(video.getSupportedWidths())
        + " height="
        + span(video.getSupportedHeights())
        + " fps="
        + span(video.getSupportedFrameRates())
        + " bitrate="
        + span(video.getBitrateRange());
  }

  private static List<String> videoCodecs(MediaCodecInfo[] infos) {
    List<String> codecs = new ArrayList<String>();
    for (MediaCodecInfo info : infos) {
      if (!hasVideoType(info) || familyOf(info) == null) {
        continue;
      }
      for (String type : info.getSupportedTypes()) {
        String capability = capability(info, type);
        if (capability != null) {
          codecs.add(capability);
        }
      }
    }
    return codecs;
  }

  private static boolean hevcHardwareEncodingSupported(
      MediaCodecInfo[] infos, int width, int height, int frameRate) {
    for (MediaCodecInfo info : infos) {
      if (!info.isEncoder()) {
        continue;
      }
      for (String type : info.getSupportedTypes()) {
        if (!type.equalsIgnoreCase("video/hevc") || isSoftware(info)) {
          continue;
        }
        MediaCodecInfo.VideoCapabilities video =
            info.getCapabilitiesForType("video/hevc").getVideoCapabilities();
        if (video != null && video.areSizeAndRateSupported(width, height, frameRate)) {
          return true;
        }
      }
    }
    return false;
  }

  private static void checkAudio(MediaCodecInfo info, List<String> expectedProfiles) {
    String type = info.getSupportedTypes()[0];
    MediaCodecInfo.CodecCapabilities capabilities = info.getCapabilitiesForType(type);
    require(capabilities.getVideoCapabilities() == null, type + " has no video capabilities");
    require(capabilities.getAudioCapabilities() != null, type + " has audio capabilities");
    require(capabilities.colorFormats.length == 0, type + " has no color formats");
    List<String> profiles = new ArrayList<String>();
    for (MediaCodecInfo.CodecProfileLevel entry : capabilities.profileLevels) {
      profiles.add(profileLevelNames(type, entry)[0]);
    }
    require(profiles.equals(expectedProfiles), type + " profiles were " + profiles);
  }

  private static void checkDecoderSet(String[] decoders, List<String> expectedVideo) {
    MediaCodecInfo[] infos = MediaCodecList.infosFor(decoders);
    require(infos.length * 2 == decoders.length, "one info per decoder pair");
    for (int i = 0; i < infos.length; i++) {
      require(infos[i].getName().equals(decoders[2 * i]), "names stay libavcodec names");
      require(
          Arrays.equals(infos[i].getSupportedTypes(), new String[] {decoders[2 * i + 1]}),
          "each decoder supports exactly its MIME type");
      require(!infos[i].isEncoder(), "no info is an encoder");
    }
    List<String> video = videoCodecs(infos);
    require(video.equals(expectedVideo), "getVideoCodecs replay returned " + video);
    require(
        !hevcHardwareEncodingSupported(infos, 1920, 1080, 30),
        "no HEVC encoder is advertised");
    checkAudio(infos[0], Arrays.asList("AACObjectLC", "AACObjectHE", "AACObjectHE_PS"));
    checkAudio(infos[infos.length - 2], Collections.<String>emptyList());
    checkAudio(infos[infos.length - 1], Collections.<String>emptyList());
  }

  private static void checkVideoCapabilities() {
    MediaCodecInfo avc = MediaCodecList.infosFor(new String[] {"h264", "video/avc"})[0];
    MediaCodecInfo.CodecCapabilities capabilities = avc.getCapabilitiesForType("video/avc");
    require(
        Arrays.equals(capabilities.colorFormats, new int[] {0x7F420888, 21}),
        "video decoders offer flexible YUV 4:2:0 and NV12");
    require(capabilities.getAudioCapabilities() == null, "video decoders have no audio");
    capabilities.colorFormats[0] = 0;
    capabilities.profileLevels[0].profile = 0;
    MediaCodecInfo.CodecCapabilities again = avc.getCapabilitiesForType("video/avc");
    require(again.colorFormats[0] == 0x7F420888, "capabilities are copies");
    require(
        again.profileLevels[0].profile == MediaCodecInfo.CodecProfileLevel.AVCProfileBaseline,
        "profile levels are copies");

    MediaCodecInfo.VideoCapabilities video = again.getVideoCapabilities();
    require(video.getWidthAlignment() == 2 && video.getHeightAlignment() == 2, "alignment 2");
    require(video.isSizeSupported(1920, 1080), "1080p is supported");
    require(video.isSizeSupported(4096, 4096), "4096x4096 is supported");
    require(!video.isSizeSupported(4098, 2), "wider than 4096 is not supported");
    require(!video.isSizeSupported(1919, 1080), "odd widths are not supported");
    require(video.areSizeAndRateSupported(1920, 1080, 60.0), "1080p60 is supported");
    require(!video.areSizeAndRateSupported(1920, 1080, 961.0), "961 fps is not supported");
    require(!video.areSizeAndRateSupported(1920, 1080, 0.5), "0.5 fps is not supported");

    try {
      avc.getCapabilitiesForType("video/hevc");
      throw new AssertionError("an unsupported type must throw");
    } catch (IllegalArgumentException expected) {
      require(expected.getMessage().contains("video/hevc"), "the message names the type");
    }
    try {
      MediaCodecList.infosFor(new String[] {"h264"});
      throw new AssertionError("an unpaired decoder name must throw");
    } catch (IllegalArgumentException expected) {
      require(expected.getMessage() != null, "the unpaired-name error has a message");
    }
  }

  private static void checkRange() {
    Range<Integer> range = Range.create(2, 8);
    require(range.contains(2) && range.contains(8) && !range.contains(9), "inclusive bounds");
    require(range.clamp(1) == 2 && range.clamp(9) == 8 && range.clamp(5) == 5, "clamp");
    require(range.intersect(Range.create(4, 10)).equals(Range.create(4, 8)), "intersect");
    require(range.intersect(Range.create(0, 20)).equals(range), "intersect with a superset");
    require(range.extend(Range.create(4, 10)).equals(Range.create(2, 10)), "extend");
    require(range.equals(new Range<Integer>(2, 8)), "equal bounds are equal ranges");
    require(range.hashCode() == Range.create(2, 8).hashCode(), "equal ranges hash alike");
    require(!range.equals(Range.create(2, 9)), "different bounds differ");
    require("[2, 8]".equals(range.toString()), "toString is [lower, upper]");
    try {
      range.intersect(Range.create(9, 10));
      throw new AssertionError("disjoint ranges must not intersect");
    } catch (IllegalArgumentException expected) {
      require(expected.getMessage() != null, "the disjoint error has a message");
    }
    try {
      Range.create(3, 2);
      throw new AssertionError("lower above upper must throw");
    } catch (IllegalArgumentException expected) {
      require(expected.getMessage() != null, "the ordering error has a message");
    }
    try {
      Range.create(null, 2);
      throw new AssertionError("a null bound must throw");
    } catch (NullPointerException expected) {
      require(expected.getMessage() != null, "the null-bound error has a message");
    }
  }

  public static void main(String[] arguments) {
    checkDecoderSet(WITH_CODECS_EXTRA, Arrays.asList(AVC_CAPABILITY, HEVC_CAPABILITY));
    checkDecoderSet(WITHOUT_CODECS_EXTRA, Collections.<String>emptyList());
    checkVideoCapabilities();
    checkRange();
    System.out.println("media-codec-list-ok");
  }
}
