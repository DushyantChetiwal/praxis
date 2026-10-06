# Praxis Remote uses org.json (part of Android) and no reflection-based
# serialization, so the app's own classes need no keep rules.

# UniFFI calls JNA by generated native symbol names, including in release builds.
-keep class org.nostrdevkit.sdk.** { *; }
-keep class com.sun.jna.** { *; }
# JNA's optional desktop window-handle helpers are never used on Android.
-dontwarn java.awt.Component
-dontwarn java.awt.GraphicsEnvironment
-dontwarn java.awt.HeadlessException
-dontwarn java.awt.Window

# OkHttp ships its own consumer rules; these cover optional TLS providers it
# probes for at runtime.
-dontwarn okhttp3.internal.platform.**
-dontwarn org.bouncycastle.**
-dontwarn org.conscrypt.**
-dontwarn org.openjsse.**

# Tink (used by androidx.security:security-crypto) references compile-only
# annotations that are not on the runtime classpath.
-dontwarn com.google.errorprone.annotations.**
-dontwarn javax.annotation.**
-dontwarn com.google.api.client.**
-dontwarn org.joda.time.**
