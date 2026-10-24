// Gradle builds for the Kotlin side of shoal-ffi (ADR 0004, ADR 0014).
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

rootProject.name = "shoal-bindings"

include(":ffi", ":jvm-test")
