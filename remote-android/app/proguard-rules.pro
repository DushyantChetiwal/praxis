# Praxis Remote uses org.json (part of Android) and no reflection-based
# serialization, so the app's own classes need no keep rules.

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
