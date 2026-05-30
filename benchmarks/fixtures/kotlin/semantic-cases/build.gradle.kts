plugins {
    kotlin("jvm") version "1.9.24"
}

dependencies {
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.1")
}

sourceSets {
    main {
        kotlin {
            srcDir("src/main/kotlin")
        }
    }
    test {
        kotlin {
            srcDir("src/test/kotlin")
        }
    }
}
