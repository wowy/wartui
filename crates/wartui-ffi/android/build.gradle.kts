// AGP compiles Kotlin itself, and through 9.5 brings in Kotlin Gradle plugin 2.2.10 unless the
// build names another on its classpath, as this does.
buildscript {
    repositories {
        mavenCentral()
    }
    dependencies {
        classpath(libs.kgp)
    }
}

plugins {
    alias(libs.plugins.android.library) apply false
}
