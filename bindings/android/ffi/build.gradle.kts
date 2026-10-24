// The shoal-ffi AAR (ADR 0014): libshoal_ffi.so for each ABI plus the uniffi-generated
// Kotlin API in org.shoalsync.ffi. Released as shoal-ffi-X.Y.Z.aar.

plugins {
    alias(libs.plugins.android.library)
}

val repo: File = rootDir.resolve("../..").canonicalFile
val workspaceVersion: String = Regex("""(?m)^version = "([^"]+)"""")
    .find(repo.resolve("Cargo.toml").readText())!!.groupValues[1]

group = "org.shoalsync"
version = workspaceVersion

// Build fewer ABIs while developing, e.g. -Pshoal.abis=x86_64.
val abis: List<String> = (findProperty("shoal.abis") as String?)
    ?.split(",")?.map { it.trim() }
    ?: listOf("arm64-v8a", "armeabi-v7a", "x86_64")
val minSdkVersion = 29
val jniLibs = layout.buildDirectory.dir("rust/jniLibs")
val generated = layout.buildDirectory.dir("generated/uniffi")
// Libraries and bindings built elsewhere, as by CI's Nix packages, instead of here:
// -Pshoal.jniLibs=<shoal-ffi-android> and -Pshoal.host=<ci-host>, whose kotlin/ holds
// the bindings.
val prebuiltJniLibs: File? = (findProperty("shoal.jniLibs") as String?)?.let(::File)
val prebuiltHost: File? = (findProperty("shoal.host") as String?)?.let(::File)

val cargoNdk by tasks.registering(Exec::class) {
    description = "Cross-compiles shoal-ffi for the Android ABIs with cargo-ndk."
    onlyIf { prebuiltJniLibs == null }
    workingDir = repo
    outputs.dir(jniLibs)
    outputs.upToDateWhen { false } // cargo decides what to rebuild
    commandLine(
        listOf("cargo", "ndk") + abis.flatMap { listOf("-t", it) } + listOf(
            "-P", minSdkVersion.toString(), "-o", jniLibs.get().asFile.path,
            "build", "-p", "shoal-ffi", "--profile", "mobile", "--locked",
        ),
    )
    // cargo-ndk also copies dependencies' cdylibs (iroh builds some); ship only ours.
    doLast {
        fileTree(jniLibs) { exclude("**/libshoal_ffi.so") }.forEach { it.delete() }
    }
}

val hostBuild by tasks.registering(Exec::class) {
    description = "Builds shoal-ffi for the host; the bindings are generated from its symbols."
    onlyIf { prebuiltHost == null }
    workingDir = repo
    outputs.upToDateWhen { false }
    commandLine("cargo", "build", "-p", "shoal-ffi", "--locked")
}

val generateBindings by tasks.registering(Exec::class) {
    description = "Generates the Kotlin API from the built library."
    onlyIf { prebuiltHost == null }
    dependsOn(cargoNdk, hostBuild)
    workingDir = repo
    outputs.dir(generated)
    outputs.upToDateWhen { false }
    doFirst { delete(generated) }
    // The Android libraries are stripped, so read the uniffi metadata from the host build.
    val library = repo.resolve("target/debug/" + System.mapLibraryName("shoal_ffi"))
    commandLine(
        "cargo", "run", "--locked", "-q", "-p", "xtask", "--bin", "uniffi-bindgen", "--",
        "generate", "--library", library.path,
        "--language", "kotlin", "--no-format", "--out-dir", generated.get().asFile.path,
    )
}

android {
    namespace = "org.shoalsync.ffi"
    compileSdk = 36

    defaultConfig {
        minSdk = minSdkVersion
        consumerProguardFiles("consumer-rules.pro")
    }
}

androidComponents {
    onVariants { variant ->
        variant.sources.java?.addStaticSourceDirectory(
            (prebuiltHost?.resolve("kotlin") ?: generated.get().asFile).path,
        )
        variant.sources.jniLibs?.addStaticSourceDirectory(
            (prebuiltJniLibs ?: jniLibs.get().asFile).path,
        )
    }
}

tasks.named("preBuild") { dependsOn(generateBindings) }

dependencies {
    // The AAR has no POM: apps declare these too (see the README).
    implementation(libs.jna) { artifact { type = "aar" } }
    implementation(libs.coroutines)
}
