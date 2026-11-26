// The Android library over `wartui-ffi`: one module, `wartui-core`, whose release AAR an app
// depends on. `../package/build.sh android` fills its generated sources and runs this build.

pluginManagement {
    repositories {
        google()
        mavenCentral()
    }
}

dependencyResolutionManagement {
    repositoriesMode = RepositoriesMode.FAIL_ON_PROJECT_REPOS
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "wartui-android"
include(":wartui-core")
