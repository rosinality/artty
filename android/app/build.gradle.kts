import java.time.Duration
import java.time.Instant

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

// Each build newer than the last: minutes since 2025 (an update installs only over an older
// versionCode). Its name says the commit, `+` if there were changes on top.
val builtMinutes = Duration.between(Instant.parse("2025-01-01T00:00:00Z"), Instant.now()).toMinutes().toInt()
fun git(vararg args: String): String = runCatching {
    val process = ProcessBuilder("git", *args).directory(rootProject.projectDir).redirectErrorStream(true).start()
    process.inputStream.bufferedReader().readText().trim().also { process.waitFor() }
}.getOrDefault("")
val commit = git("rev-parse", "--short", "HEAD").ifEmpty { "unknown" }
val changed = git("status", "--porcelain", "--untracked-files=no").isNotEmpty()
val builtName = "0.1.0 ($commit${if (changed) "+" else ""})"

// The release key: a copy of this Mac's debug key (signing/, never committed), so a release
// installs over the debug builds already on a phone. Updates install only over an app signed
// with the same key: keep it.
val releaseKey = rootProject.file("signing/artty.keystore")

android {
    namespace = "io.github.rosinality.artty"
    compileSdk = 37

    defaultConfig {
        applicationId = "io.github.rosinality.artty"
        minSdk = 28
        targetSdk = 36
        versionCode = builtMinutes
        versionName = builtName
    }

    signingConfigs {
        create("release") {
            storeFile = releaseKey
            storePassword = "android"
            keyAlias = "androiddebugkey"
            keyPassword = "android"
        }
    }

    buildTypes {
        debug {
            // Phones, and emulators on either kind of Mac.
            ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
        }
        release {
            // Phones are arm64: half the native code, and the rest shrunk (R8).
            ndk { abiFilters += listOf("arm64-v8a") }
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            if (releaseKey.exists()) signingConfig = signingConfigs.getByName("release")
        }
    }

    buildFeatures {
        compose = true
    }

    sourceSets {
        getByName("main") {
            // UniFFI's Kotlin for the Rust side (build-rust.sh).
            kotlin.directories.add("build/generated/uniffi")
            // This app's build of artty for Linux (build-rust.sh), to install on servers.
            assets.directories.add("build/generated/linux-assets")
        }
    }
}

// The Rust side: the native library for each ABI, and its Kotlin.
val buildRust = tasks.register<Exec>("buildRust") {
    workingDir = rootProject.projectDir
    commandLine("sh", "build-rust.sh")
}
tasks.named("preBuild") { dependsOn(buildRust) }

// What publish.sh puts in version.json.
tasks.register("releaseVersion") {
    val out = layout.buildDirectory.file("release-version.properties")
    outputs.file(out)
    outputs.upToDateWhen { false }
    doLast { out.get().asFile.writeText("versionCode=$builtMinutes\nversionName=$builtName\n") }
}

dependencies {
    val compose = platform("androidx.compose:compose-bom:2026.09.00")
    implementation(compose)
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("net.java.dev.jna:jna:5.19.1@aar")
}
