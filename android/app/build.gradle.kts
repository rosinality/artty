plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "io.github.rosinality.artty"
    compileSdk = 37

    defaultConfig {
        applicationId = "io.github.rosinality.artty"
        minSdk = 28
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    buildFeatures {
        compose = true
    }

    sourceSets {
        getByName("main") {
            // UniFFI's Kotlin for the Rust side (build-rust.sh).
            kotlin.directories.add("build/generated/uniffi")
        }
    }
}

// The Rust side: the native library for each ABI, and its Kotlin.
val buildRust = tasks.register<Exec>("buildRust") {
    workingDir = rootProject.projectDir
    commandLine("sh", "build-rust.sh")
}
tasks.named("preBuild") { dependsOn(buildRust) }

dependencies {
    val compose = platform("androidx.compose:compose-bom:2026.09.00")
    implementation(compose)
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("net.java.dev.jna:jna:5.19.1@aar")
}
