import java.lang.reflect.InvocationTargetException;
import java.util.TimeZone;

public final class TimeZoneProbe {
    private static final long MID_JANUARY_2026_UTC = 1768478400000L;
    private static final long JULY_2026_UTC = 1782907200000L;
    private static final int MINUTE_MILLIS = 60000;

    private static void require(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }

    private static void requireOffsets(TimeZone zone, String id, int winterMinutes, int summerMinutes) {
        require(zone.getID().equals(id), "expected " + id + ", got " + zone.getID());
        require(zone.getOffset(MID_JANUARY_2026_UTC) == winterMinutes * MINUTE_MILLIS,
                id + " January offset is " + zone.getOffset(MID_JANUARY_2026_UTC));
        require(zone.getOffset(JULY_2026_UTC) == summerMinutes * MINUTE_MILLIS,
                id + " July offset is " + zone.getOffset(JULY_2026_UTC));
    }

    public static void main(String[] arguments) throws ReflectiveOperationException {
        requireOffsets(TimeZone.getDefault(), "America/Los_Angeles", -480, -420);
        requireOffsets(TimeZone.getTimeZone("Europe/Paris"), "Europe/Paris", 60, 120);
        requireOffsets(TimeZone.getTimeZone("Asia/Calcutta"), "Asia/Calcutta", 330, 330);
        Object tzdata = Class.forName("libcore.util.ZoneInfoDB").getMethod("getInstance").invoke(null);
        try {
            tzdata.getClass().getMethod("validate").invoke(tzdata);
        } catch (InvocationTargetException error) {
            throw new AssertionError("the tzdata bundle does not load every zone", error.getCause());
        }
        System.out.println("time-zone-ok");
        Runtime.getRuntime().halt(0);
    }
}
