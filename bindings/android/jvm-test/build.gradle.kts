// Runs the generated Kotlin bindings on the JVM against the host build of tidefell-ffi, and a
// real tidefelld, so the Kotlin surface is tested in monorepo CI without an Android device.

plugins {
    alias(libs.plugins.kotlin.jvm)
}

val repo: File = rootDir.resolve("../..").canonicalFile
val cargoTarget: File = repo.resolve("target/debug")
val generated = layout.buildDirectory.dir("generated/uniffi")

val cargoBuild by tasks.registering(Exec::class) {
    description = "Builds tidefell-ffi, tidefelld and tidefell for the host."
    workingDir = repo
    commandLine(
        "cargo", "build", "--locked",
        "-p", "tidefell-ffi", "-p", "tidefell-daemon", "-p", "tidefell-cli",
    )
}

val generateBindings by tasks.registering(Exec::class) {
    description = "Generates the Kotlin bindings from the built library."
    dependsOn(cargoBuild)
    workingDir = repo
    val library = cargoTarget.resolve(System.mapLibraryName("tidefell_ffi"))
    inputs.file(library)
    outputs.dir(generated)
    doFirst { delete(generated) }
    commandLine(
        "cargo", "run", "--locked", "-q", "-p", "xtask", "--bin", "uniffi-bindgen", "--",
        "generate", "--library", library.path, "--language", "kotlin", "--no-format",
        "--out-dir", generated.get().asFile.path,
    )
}

kotlin {
    jvmToolchain(21)
    sourceSets["test"].kotlin.srcDir(generateBindings)
}

dependencies {
    testImplementation(libs.coroutines)
    testImplementation(libs.jna)
    testImplementation(platform(libs.junit.bom))
    testImplementation(libs.junit.jupiter)
    testImplementation(kotlin("test"))
    testRuntimeOnly(libs.junit.launcher)
}

tasks.test {
    useJUnitPlatform()
    dependsOn(cargoBuild)
    systemProperty("jna.library.path", cargoTarget.path)
    systemProperty("tidefell.bin", cargoTarget.path)
    testLogging {
        events("passed", "failed")
        showStandardStreams = true
        exceptionFormat = org.gradle.api.tasks.testing.logging.TestExceptionFormat.FULL
    }
}
