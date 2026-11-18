# The Rust engine calls this by name: Progress.onProgress(JJ)Z.
-keep interface io.github.tyetladd.wavo.Native$Progress { *; }
-keep class io.github.tyetladd.wavo.Native { *; }

# sherpa-onnx's JNI library reads its config classes' fields and builds its results by name.
-keep class com.k2fsa.sherpa.onnx.** { *; }
