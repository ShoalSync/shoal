// Runs the generated Kotlin bindings on the JVM against the host build of tidefell-ffi, and a
// real tidefelld, so the Kotlin surface is tested in monorepo CI without an Android device.

plugins {
    alias(libs.plugins.kotlin.jvm)
}

val repo: File = rootDir.resolve("../..").canonicalFile
val cargoTarget: File = repo.resolve("target/debug")
val generated = layout.buildDirectory.dir("generated/uniffi")
// A host build made elsewhere, as CI's Nix package `ci-host` is (-Ptidefell.host=<its path>):
// its lib/, bin/ and generated kotlin/ instead of building them here.
val prebuilt: File? = (findProperty("tidefell.host") as String?)?.let(::File)
val libDir: File = prebuilt?.resolve("lib") ?: cargoTarget
val binDir: File = prebuilt?.resolve("bin") ?: cargoTarget

val cargoBuild by tasks.registering(Exec::class) {
    description = "Builds tidefell-ffi, tidefelld and tidefell for the host."
    onlyIf { prebuilt == null }
    workingDir = repo
    commandLine(
        "cargo", "build", "--locked",
        "-p", "tidefell-ffi", "-p", "tidefell-daemon", "-p", "tidefell-cli",
    )
}

val generateBindings by tasks.registering(Exec::class) {
    description = "Generates the Kotlin bindings from the built library."
    onlyIf { prebuilt == null }
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
    sourceSets["test"].kotlin.srcDir(prebuilt?.resolve("kotlin") ?: generateBindings)
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
    systemProperty("jna.library.path", libDir.path)
    systemProperty("tidefell.bin", binDir.path)
    testLogging {
        events("passed", "failed")
        showStandardStreams = true
        exceptionFormat = org.gradle.api.tasks.testing.logging.TestExceptionFormat.FULL
    }
}
