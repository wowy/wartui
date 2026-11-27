import org.jetbrains.kotlin.gradle.dsl.JvmTarget

// The AAR: the generated binding and `libwartui_ffi.so` for each ABI, both laid under
// `generated/` by `package/build.sh`, which git ignores. No hand-written source ships in it.
plugins {
    alias(libs.plugins.android.library)
    alias(libs.plugins.android.junit)
}

val jvm = libs.versions.jvm.get()

android {
    namespace = "racing.deadbear.wartui.core"
    compileSdk = libs.versions.compileSdk.get().toInt()

    defaultConfig {
        minSdk = libs.versions.minSdk.get().toInt()
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.toVersion(jvm)
        targetCompatibility = JavaVersion.toVersion(jvm)
    }

    // `build.sh` strips the library with the NDK that linked it; this build needs no NDK.
    packaging {
        jniLibs {
            keepDebugSymbols += "**/libwartui_ffi.so"
        }
    }

    sourceSets.named("main") {
        kotlin.directories += "generated/kotlin"
        jniLibs.directories += "generated/jniLibs"
    }
}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.fromTarget(jvm)
    }
}

dependencies {
    // An app calls the binding through both, so they come with it.
    api(libs.jna) { artifact { type = "aar" } }
    api(libs.coroutines)
    // The binding marks its API 34 cleaner with `@RequiresApi`.
    implementation(libs.annotation)

    // The test is JUnit 5. The `android-junit` plugin adds its runner, which `AndroidJUnitRunner`
    // starts; that runner is built on JUnit 4 and still brings `junit:junit` with it.
    androidTestImplementation(libs.jupiter.api)
    androidTestImplementation(libs.androidx.test.runner)
}
