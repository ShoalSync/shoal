// The tidefell-ffi AAR (ADR 0014): libtidefell_ffi.so for each ABI plus the uniffi-generated
// Kotlin API in org.tidefell.ffi. Released as tidefell-ffi-X.Y.Z.aar.

plugins {
    alias(libs.plugins.android.library)
}

val repo: File = rootDir.resolve("../..").canonicalFile
val workspaceVersion: String = Regex("""(?m)^version = "([^"]+)"""")
    .find(repo.resolve("Cargo.toml").readText())!!.groupValues[1]

group = "org.tidefell"
version = workspaceVersion

// Build fewer ABIs while developing, e.g. -Ptidefell.abis=x86_64.
val abis: List<String> = (findProperty("tidefell.abis") as String?)
    ?.split(",")?.map { it.trim() }
    ?: listOf("arm64-v8a", "armeabi-v7a", "x86_64")
val minSdkVersion = 29
val jniLibs = layout.buildDirectory.dir("rust/jniLibs")
val generated = layout.buildDirectory.dir("generated/uniffi")

val cargoNdk by tasks.registering(Exec::class) {
    description = "Cross-compiles tidefell-ffi for the Android ABIs with cargo-ndk."
    workingDir = repo
    outputs.dir(jniLibs)
    outputs.upToDateWhen { false } // cargo decides what to rebuild
    commandLine(
        listOf("cargo", "ndk") + abis.flatMap { listOf("-t", it) } + listOf(
            "-P", minSdkVersion.toString(), "-o", jniLibs.get().asFile.path,
            "build", "-p", "tidefell-ffi", "--profile", "mobile", "--locked",
        ),
    )
    // cargo-ndk also copies dependencies' cdylibs (iroh builds some); ship only ours.
    doLast {
        fileTree(jniLibs) { exclude("**/libtidefell_ffi.so") }.forEach { it.delete() }
    }
}

val hostBuild by tasks.registering(Exec::class) {
    description = "Builds tidefell-ffi for the host; the bindings are generated from its symbols."
    workingDir = repo
    outputs.upToDateWhen { false }
    commandLine("cargo", "build", "-p", "tidefell-ffi", "--locked")
}

val generateBindings by tasks.registering(Exec::class) {
    description = "Generates the Kotlin API from the built library."
    dependsOn(cargoNdk, hostBuild)
    workingDir = repo
    outputs.dir(generated)
    outputs.upToDateWhen { false }
    doFirst { delete(generated) }
    // The Android libraries are stripped, so read the uniffi metadata from the host build.
    val library = repo.resolve("target/debug/" + System.mapLibraryName("tidefell_ffi"))
    commandLine(
        "cargo", "run", "--locked", "-q", "-p", "xtask", "--bin", "uniffi-bindgen", "--",
        "generate", "--library", library.path,
        "--language", "kotlin", "--no-format", "--out-dir", generated.get().asFile.path,
    )
}

android {
    namespace = "org.tidefell.ffi"
    compileSdk = 36

    defaultConfig {
        minSdk = minSdkVersion
        consumerProguardFiles("consumer-rules.pro")
    }
}

androidComponents {
    onVariants { variant ->
        variant.sources.java?.addStaticSourceDirectory(generated.get().asFile.path)
        variant.sources.jniLibs?.addStaticSourceDirectory(jniLibs.get().asFile.path)
    }
}

tasks.named("preBuild") { dependsOn(generateBindings) }

dependencies {
    // The AAR has no POM: apps declare these too (see the README).
    implementation(libs.jna) { artifact { type = "aar" } }
    implementation(libs.coroutines)
}
