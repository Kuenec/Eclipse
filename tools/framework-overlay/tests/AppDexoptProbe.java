public final class AppDexoptProbe {
    public static void main(String[] arguments) throws ClassNotFoundException {
        Class<?> shadowed = Class.forName("android.webkit.ValueCallback");
        if (!shadowed.isInterface()) {
            throw new AssertionError("android.webkit.ValueCallback resolved to a class");
        }
        System.out.println("app-dexopt-probe-ok");
        Runtime.getRuntime().halt(0);
    }
}
