import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
}

// Everything CI provides is optional, so a plain local debug build still works.
fun env(name: String): String? = System.getenv(name)?.trim()?.takeIf { it.isNotEmpty() }

val remoteVersionCode = env("PRAXIS_REMOTE_VERSION_CODE")?.toIntOrNull() ?: 1
val remoteVersionName = env("PRAXIS_REMOTE_VERSION_NAME") ?: "0.1.0-dev"
val githubClientId = env("PRAXIS_REMOTE_CLIENT_ID") ?: ""
val keystorePath = env("ANDROID_KEYSTORE_PATH")

fun javaString(value: String): String =
    "\"" + value.replace("\\", "\\\\").replace("\"", "\\\"") + "\""

android {
    namespace = "io.github.dushyantchetiwal.praxis.remote"
    compileSdk = 35

    defaultConfig {
        applicationId = "io.github.dushyantchetiwal.praxis.remote"
        minSdk = 26
        targetSdk = 35
        versionCode = remoteVersionCode
        versionName = remoteVersionName
        buildConfigField("String", "GITHUB_CLIENT_ID", javaString(githubClientId))
    }

    signingConfigs {
        if (keystorePath != null) {
            create("release") {
                storeFile = file(keystorePath)
                storePassword = env("ANDROID_KEYSTORE_PASSWORD")
                keyAlias = env("ANDROID_KEY_ALIAS")
                keyPassword = env("ANDROID_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
            if (keystorePath != null) {
                signingConfig = signingConfigs.getByName("release")
            }
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    packaging {
        resources {
            excludes += "/META-INF/{AL2.0,LGPL2.1}"
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(JvmTarget.JVM_17)
    }
}

dependencies {
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.material3)
    implementation(libs.androidx.compose.material.icons.extended)
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(libs.kotlinx.coroutines.android)
    implementation(libs.okhttp)
    implementation(libs.androidx.security.crypto)

    testImplementation(libs.junit)
    testImplementation(libs.org.json)
}
