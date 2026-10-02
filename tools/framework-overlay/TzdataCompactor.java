import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

public final class TzdataCompactor {
    private static final int ID_BYTES = 40;
    private static final int HEADER_BYTES = 24;
    private static final int INDEX_ENTRY_BYTES = ID_BYTES + 12;
    private static final int TZIF_HEADER_BYTES = 44;
    private static final int TZIF_V1_TIMECNT_OFFSET = 32;
    private static final String VERSION_LINE_PREFIX = "# version ";
    private static final Pattern IANA_VERSION = Pattern.compile("(\\d{4}[a-z])\\b.*");
    private static final String UNKNOWN_VERSION = "?????";

    private TzdataCompactor() {}

    public static void main(String[] arguments) throws IOException {
        if (arguments.length != 2) {
            throw new IllegalArgumentException("usage: TzdataCompactor <zoneinfo dir> <tzdata out>");
        }
        Path zoneinfo = Paths.get(arguments[0]);
        Path tzdataZi = zoneinfo.resolve("tzdata.zi");
        List<String> lines = Files.readAllLines(tzdataZi, StandardCharsets.US_ASCII);
        String versionLine = lines.isEmpty() ? "" : lines.get(0);
        if (!versionLine.startsWith(VERSION_LINE_PREFIX)) {
            throw new IOException(tzdataZi + " does not start with a '" + VERSION_LINE_PREFIX
                    + "' line");
        }
        Matcher version =
                IANA_VERSION.matcher(versionLine.substring(VERSION_LINE_PREFIX.length()));
        String tzdataVersion = version.matches() ? version.group(1) : UNKNOWN_VERSION;

        TreeMap<String, byte[]> zones = new TreeMap<String, byte[]>();
        boolean any32BitTransitions = false;
        for (String line : lines) {
            String[] fields = line.split("\\s+");
            String id;
            if (fields[0].equals("Z") && fields.length > 1) {
                id = fields[1];
            } else if (fields[0].equals("L") && fields.length > 2) {
                id = fields[2];
            } else {
                continue;
            }
            byte[] tzif = Files.readAllBytes(zoneinfo.resolve(id));
            if (tzif.length < TZIF_HEADER_BYTES || tzif[0] != 'T' || tzif[1] != 'Z'
                    || tzif[2] != 'i' || tzif[3] != 'f') {
                throw new IOException(zoneinfo.resolve(id) + " is not a TZif file");
            }
            if (id.length() >= ID_BYTES) {
                throw new IOException("zone id " + id + " does not fit Android's "
                        + ID_BYTES + "-byte tzdata index");
            }
            any32BitTransitions |= readInt(tzif, TZIF_V1_TIMECNT_OFFSET) > 0;
            zones.put(id, tzif);
        }
        if (!any32BitTransitions) {
            throw new IOException(zoneinfo + " holds slim TZif files without 32-bit transitions,"
                    + " which Android's tzdata format needs; set ZONEINFO_DIR to fat TZif files");
        }
        byte[] zoneTab = Files.readAllBytes(zoneinfo.resolve("zone.tab"));

        ByteArrayOutputStream data = new ByteArrayOutputStream();
        Map<String, Integer> dataOffsets = new HashMap<String, Integer>();
        ByteArrayOutputStream index = new ByteArrayOutputStream();
        DataOutputStream indexOut = new DataOutputStream(index);
        for (Map.Entry<String, byte[]> zone : zones.entrySet()) {
            byte[] tzif = zone.getValue();
            String content = new String(tzif, StandardCharsets.ISO_8859_1);
            Integer offset = dataOffsets.get(content);
            if (offset == null) {
                offset = data.size();
                dataOffsets.put(content, offset);
                data.write(tzif);
            }
            byte[] id = new byte[ID_BYTES];
            byte[] name = zone.getKey().getBytes(StandardCharsets.US_ASCII);
            System.arraycopy(name, 0, id, 0, name.length);
            indexOut.write(id);
            indexOut.writeInt(offset);
            indexOut.writeInt(tzif.length);
            indexOut.writeInt(0);
        }

        int dataOffset = HEADER_BYTES + zones.size() * INDEX_ENTRY_BYTES;
        ByteArrayOutputStream tzdata = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(tzdata);
        out.write(("tzdata" + tzdataVersion).getBytes(StandardCharsets.US_ASCII));
        out.writeByte(0);
        out.writeInt(HEADER_BYTES);
        out.writeInt(dataOffset);
        out.writeInt(dataOffset + data.size());
        index.writeTo(out);
        data.writeTo(out);
        out.write(zoneTab);
        Files.write(Paths.get(arguments[1]), tzdata.toByteArray());
    }

    private static int readInt(byte[] bytes, int offset) {
        return ((bytes[offset] & 0xff) << 24) | ((bytes[offset + 1] & 0xff) << 16)
                | ((bytes[offset + 2] & 0xff) << 8) | (bytes[offset + 3] & 0xff);
    }
}
