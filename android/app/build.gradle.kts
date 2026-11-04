// SPDX-License-Identifier: GPL-3.0-or-later

import javax.inject.Inject

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
}

android {
    namespace = "io.github.nexoryx.pasteget"
    // Compose 1.12 needs to be compiled against API 37.2. The app still targets API 36 and
    // runs from Android 10 (minSdk 29).
    compileSdk = 37
    compileSdkMinor = 2
    ndkVersion = "30.0.16248370"

    defaultConfig {
        applicationId = "io.github.nexoryx.pasteget"
        minSdk = 29
        targetSdk = 36
        versionCode = 1
        versionName = "0.0.0"

        // The ABIs the Rust core is built for (see cargoNdkBuild). Other ABIs would install, then
        // crash when loading it.
        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    // `foss`: only free software, for F-Droid. A `full` flavor may add optional extras later.
    flavorDimensions += "distribution"
    productFlavors {
        create("foss") {
            dimension = "distribution"
        }
    }

    signingConfigs {
        // Committed on purpose: every debug build, local or CI, gets the same signature, so a
        // newer debug APK installs over an older one. Never used for releases.
        getByName("debug") {
            storeFile = file("debug.keystore")
            storePassword = "android"
            keyAlias = "androiddebugkey"
            keyPassword = "android"
        }
        // The release key never enters the repository: CI decodes it from a secret and passes
        // its path and passwords in the environment (docs/RELEASING.md). Without them, release
        // builds come out unsigned.
        val keystore = System.getenv("PASTEGET_KEYSTORE")
        if (keystore != null) {
            create("release") {
                storeFile = file(keystore)
                storePassword = System.getenv("PASTEGET_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("PASTEGET_KEY_ALIAS")
                keyPassword = System.getenv("PASTEGET_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        getByName("release") {
            signingConfig = signingConfigs.findByName("release")
            // No shrinking: JNA and the generated bindings find their classes by name.
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
        // IShellClipboard: the app talks to its Shizuku service through AIDL.
        aidl = true
    }

    lint {
        warningsAsErrors = true
    }

    testOptions {
        // Robolectric tests run against the app's real resources and merged manifest.
        unitTests.isIncludeAndroidResources = true
        // Robolectric's Android 16 reaches into file descriptors, which JDK 17+ closes off.
        unitTests.all { it.jvmArgs("--add-opens=java.base/jdk.internal.access=ALL-UNNAMED") }
    }
}

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(platform(libs.compose.bom))
    implementation(libs.compose.material3)
    implementation(libs.compose.ui)
    implementation(libs.compose.ui.tooling.preview)
    implementation(libs.kotlinx.coroutines.android)
    implementation(libs.zxing.embedded)
    // Shizuku (MIT): runs ShellClipboardService as the shell user, for the automatic copy.
    implementation(libs.shizuku.api)
    implementation(libs.shizuku.provider)
    // The generated bindings call the Rust library through JNA.
    implementation(libs.jna) { artifact { type = "aar" } }

    testImplementation(libs.junit)
    // Android framework classes on the JVM, for tests of code that uses them (MIT).
    testImplementation(libs.robolectric)
}

// --- Rust core -------------------------------------------------------------------------------
// The app's native library is `pasteget-ffi` from the Cargo workspace one directory up, built
// with cargo-ndk. Its Kotlin bindings are generated from the built library by uniffi-bindgen.

val cargoWorkspace: File = rootProject.projectDir.parentFile

/** Builds `libpasteget_ffi.so` for every ABI the app ships. */
abstract class CargoNdkBuild @Inject constructor(private val exec: ExecOperations) : DefaultTask() {
    @get:Internal
    abstract val workspace: DirectoryProperty

    @get:Input
    abstract val ndkDirectory: Property<String>

    @get:Input
    abstract val abis: ListProperty<String>

    @get:OutputDirectory
    abstract val jniLibs: DirectoryProperty

    init {
        // Cargo tracks its own inputs and returns quickly when nothing changed.
        outputs.upToDateWhen { false }
    }

    @TaskAction
    fun build() {
        val targets = abis.get().flatMap { listOf("-t", it) }
        exec.exec {
            workingDir = workspace.get().asFile
            environment("ANDROID_NDK_HOME", ndkDirectory.get())
            commandLine(
                listOf("cargo", "ndk") + targets + listOf(
                    "--platform", "29",
                    "-o", jniLibs.get().asFile.absolutePath,
                    "build", "--release", "-p", "pasteget-ffi",
                ),
            )
        }
    }
}

/** Generates the Kotlin bindings from the built library. */
abstract class UniffiBindgen @Inject constructor(private val exec: ExecOperations) : DefaultTask() {
    @get:Internal
    abstract val workspace: DirectoryProperty

    @get:InputFile
    abstract val library: RegularFileProperty

    @get:OutputDirectory
    abstract val kotlinSources: DirectoryProperty

    @TaskAction
    fun generate() {
        exec.exec {
            workingDir = workspace.get().asFile
            commandLine(
                "cargo", "run", "--quiet", "-p", "uniffi-bindgen", "--",
                "generate",
                "--library", library.get().asFile.absolutePath,
                "--language", "kotlin",
                "--no-format",
                "--out-dir", kotlinSources.get().asFile.absolutePath,
            )
        }
    }
}

val cargoNdkBuild = tasks.register<CargoNdkBuild>("cargoNdkBuild") {
    workspace.set(cargoWorkspace)
    ndkDirectory.set(androidComponents.sdkComponents.ndkDirectory.map { it.asFile.absolutePath })
    abis.set(listOf("arm64-v8a", "x86_64"))
    jniLibs.set(layout.buildDirectory.dir("rust/jniLibs"))
}

val uniffiBindgen = tasks.register<UniffiBindgen>("uniffiBindgen") {
    workspace.set(cargoWorkspace)
    library.set(cargoNdkBuild.flatMap { it.jniLibs.file("arm64-v8a/libpasteget_ffi.so") })
    kotlinSources.set(layout.buildDirectory.dir("generated/uniffi"))
}

androidComponents {
    onVariants { variant ->
        variant.sources.jniLibs?.addGeneratedSourceDirectory(cargoNdkBuild, CargoNdkBuild::jniLibs)
        variant.sources.kotlin?.addGeneratedSourceDirectory(uniffiBindgen, UniffiBindgen::kotlinSources)
    }
}
