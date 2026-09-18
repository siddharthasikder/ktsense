plugins {
    kotlin("jvm")
}

dependencies {
    implementation(project(":core"))
    implementation(project(":db"))
}

kotlin {
    jvmToolchain(17)
}
