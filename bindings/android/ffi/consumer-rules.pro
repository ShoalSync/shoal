# JNA and the generated bindings are reached through reflection.
-keep class com.sun.jna.** { *; }
-keep class * implements com.sun.jna.** { *; }
-keep class org.shoalsync.ffi.** { *; }
-dontwarn java.awt.**
