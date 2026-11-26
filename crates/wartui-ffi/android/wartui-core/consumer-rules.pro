# What an app's R8 keeps of JNA and the binding, which both reach by reflection.

# JNA, as its FAQ gives them ("JNA on Android").
-dontwarn java.awt.*
-keep class com.sun.jna.* { *; }
-keep class * extends com.sun.jna.* { *; }
-keepclassmembers class * extends com.sun.jna.* { public *; }

# JNA binds `UniffiLib`'s native methods to the library's symbols by name, and reads each
# `Structure`'s field order from its `@FieldOrder` annotation.
-keepclasseswithmembernames class racing.deadbear.wartui.core.** { native <methods>; }
-keepattributes RuntimeVisibleAnnotations
