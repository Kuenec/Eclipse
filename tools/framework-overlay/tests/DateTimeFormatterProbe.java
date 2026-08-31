import java.time.LocalDateTime;
import java.time.format.DateTimeFormatter;

public final class DateTimeFormatterProbe {
    public static void main(String[] arguments) {
        String actual = DateTimeFormatter.ofPattern("yyyy-MM-dd HH:mm:ss")
                .format(LocalDateTime.of(2026, 8, 30, 13, 0, 0));
        String expected = "2026-08-30 13:00:00";
        if (!expected.equals(actual)) {
            throw new AssertionError("expected " + expected + ", got " + actual);
        }
        System.out.println(actual);
        Runtime.getRuntime().halt(0);
    }
}
