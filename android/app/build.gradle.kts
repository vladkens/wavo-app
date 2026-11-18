import java.security.MessageDigest
import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

// The ABIs the engine is built for: `-Pwavo.abis=arm64-v8a,x86_64` adds the emulator's.
val abis = (findProperty("wavo.abis") as String? ?: "arm64-v8a").split(",")

android {
    namespace = "io.github.tyetladd.wavo"
    compileSdk = 37
    ndkVersion = "26.1.10909125"

    defaultConfig {
        applicationId = "io.github.tyetladd.wavo"
        minSdk = 29
        targetSdk = 36
        versionCode = 9
        versionName = "0.4.2"
        ndk { abiFilters += abis }
    }

    // The release key, when ~/.gradle/gradle.properties or -P names one: wavo.keystore (a path),
    // wavo.keystore.password, wavo.key.alias and wavo.key.password. Without it the release is
    // signed with this machine's debug key: installable, not for a store, and a build from
    // another machine won't install over it.
    val keystore = findProperty("wavo.keystore") as String?
    signingConfigs {
        if (keystore != null) {
            create("release") {
                storeFile = file(keystore)
                storePassword = findProperty("wavo.keystore.password") as String?
                keyAlias = findProperty("wavo.key.alias") as String?
                keyPassword = findProperty("wavo.key.password") as String?
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            signingConfig = signingConfigs.getByName(if (keystore != null) "release" else "debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures { compose = true }

    // Of sherpa-onnx only its JNI library and ONNX Runtime are used.
    packaging {
        jniLibs.excludes += listOf("**/libsherpa-onnx-c-api.so", "**/libsherpa-onnx-cxx-api.so")
    }
}

// AGP 9 compiles Kotlin itself, so the JVM target is set here, not in android.kotlinOptions.
kotlin {
    compilerOptions {
        jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17)
    }
}

// sherpa-onnx, for the speakers of a conversation: its AAR is only on GitHub's releases, and no
// binary is kept in git, so it is downloaded once into libs/ and checked.
val sherpaVersion = "1.13.8"
val sherpaSha256 = "633c24321e06b1fe79feafa03ea16cbc0f8a286641e2da3559bac91bdb13bd96"
val sherpaAar = layout.projectDirectory.file("libs/sherpa-onnx-$sherpaVersion.aar")

val sherpa = tasks.register("sherpaAar") {
    outputs.file(sherpaAar)
    doLast {
        val file = sherpaAar.asFile
        file.parentFile.mkdirs()
        val url = "https://github.com/k2-fsa/sherpa-onnx/releases/download/v$sherpaVersion/sherpa-onnx-$sherpaVersion.aar"
        uri(url).toURL().openStream().use { input -> file.outputStream().use { input.copyTo(it) } }

        val digest = MessageDigest.getInstance("SHA-256")
        file.inputStream().use { input ->
            val buffer = ByteArray(1 shl 16)
            while (true) {
                val n = input.read(buffer)
                if (n < 0) break
                digest.update(buffer, 0, n)
            }
        }
        val sha256 = digest.digest().joinToString("") { byte -> String.format("%02x", byte) }
        if (sha256 != sherpaSha256) {
            file.delete()
            throw GradleException("sherpa-onnx $sherpaVersion: sha256 is $sha256, not $sherpaSha256")
        }
    }
}

// The engine: core/ built by cargo-ndk into jniLibs, before anything else.
// AGP 9 dropped ndkDirectory, so the NDK is found under the SDK at ndk/<ndkVersion>.
val sdkDir = System.getenv("ANDROID_HOME")
    ?: System.getenv("ANDROID_SDK_ROOT")
    ?: rootProject.file("local.properties").takeIf { it.exists() }?.let { props ->
        Properties().apply { props.inputStream().use { load(it) } }.getProperty("sdk.dir")
    }
    ?: throw GradleException("No Android SDK: set ANDROID_HOME or write sdk.dir in android/local.properties")

val ndkHome = File(sdkDir, "ndk/${android.ndkVersion}")

val cargoNdk = tasks.register<Exec>("cargoNdk") {
    workingDir = rootProject.file("core")
    // The old ndkDirectory accessor also had AGP install the NDK; a path of our own does not, so
    // say what is missing rather than let cargo fail with "No such file or directory".
    doFirst {
        if (!ndkHome.isDirectory) {
            throw GradleException(
                "NDK ${android.ndkVersion} is not installed at $ndkHome.\n" +
                    "Install it: sdkmanager --install \"ndk;${android.ndkVersion}\"",
            )
        }
    }
    // Both, because a runner may already point ANDROID_NDK_ROOT at an NDK of its own.
    environment("ANDROID_NDK_HOME", ndkHome.path)
    environment("ANDROID_NDK_ROOT", ndkHome.path)
    commandLine(
        listOf("cargo", "ndk") + abis.flatMap { listOf("-t", it) } +
            listOf("--platform", "29", "-o", file("src/main/jniLibs").path, "build", "--release", "--locked"),
    )
}

tasks.named("preBuild") { dependsOn(cargoNdk, sherpa) }

dependencies {
    implementation(files(sherpaAar))
    // Pinned as the sshmanager project pins them, so one copy of each serves both builds.
    implementation("androidx.compose.ui:ui:1.9.3")
    implementation("androidx.compose.material3:material3:1.4.0")
    implementation("androidx.compose.material:material-icons-core:1.7.8")
    implementation("androidx.activity:activity-compose:1.10.1")
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.11.0")

    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20250517")
}
