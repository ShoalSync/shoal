// Gradle builds for the Kotlin side of tidefell-ffi (ADR 0004, ADR 0014).
pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "tidefell-bindings"

include(":ffi", ":jvm-test")
