import 'package:flutter/material.dart';

abstract final class ConnectoColors {
  static const bg = Color(0xff0d1015);
  static const surface = Color(0xff14181e);
  static const raised = Color(0xff1a1f27);
  static const hover = Color(0xff20262f);
  static const border = Color(0xff262c36);
  static const borderStrong = Color(0xff353c48);
  static const text = Color(0xffe7eaee);
  static const muted = Color(0xffa0a8b4);
  static const faint = Color(0xff6e7682);
  static const accent = Color(0xff5fe3ea);
  static const onAccent = Color(0xff061316);
  static const userBubble = Color(0xff15292d);
  static const userBorder = Color(0xff224a50);
  static const coral = Color(0xfff0645a);
  static const amber = Color(0xfff2b84b);
  static const green = Color(0xff46cf8b);
}

const monoStyle = TextStyle(
  fontFamily: 'Menlo',
  fontFamilyFallback: ['Menlo', 'SF Mono', 'Consolas', 'monospace'],
);

ThemeData connectoTheme({String? fontFamily}) {
  const scheme = ColorScheme.dark(
    primary: ConnectoColors.accent,
    onPrimary: ConnectoColors.onAccent,
    secondary: ConnectoColors.accent,
    onSecondary: ConnectoColors.onAccent,
    secondaryContainer: ConnectoColors.userBubble,
    onSecondaryContainer: ConnectoColors.text,
    surface: ConnectoColors.surface,
    onSurface: ConnectoColors.text,
    onSurfaceVariant: ConnectoColors.muted,
    surfaceContainer: ConnectoColors.raised,
    surfaceContainerHighest: ConnectoColors.hover,
    outline: ConnectoColors.borderStrong,
    outlineVariant: ConnectoColors.border,
    error: ConnectoColors.coral,
    onError: ConnectoColors.bg,
  );
  const shape = RoundedRectangleBorder(
    borderRadius: BorderRadius.all(Radius.circular(8)),
  );
  final focusBorder = WidgetStateProperty.resolveWith<BorderSide?>((states) {
    if (states.contains(WidgetState.focused)) {
      return const BorderSide(color: ConnectoColors.accent, width: 2);
    }
    return null;
  });
  const label = TextStyle(fontSize: 12, fontWeight: FontWeight.w500);
  final button = ButtonStyle(
    elevation: const WidgetStatePropertyAll(0),
    minimumSize: const WidgetStatePropertyAll(Size(36, 36)),
    padding: const WidgetStatePropertyAll(EdgeInsets.symmetric(horizontal: 14)),
    shape: const WidgetStatePropertyAll(shape),
    side: focusBorder,
    textStyle: WidgetStatePropertyAll(label.copyWith(fontFamily: fontFamily)),
  );
  const fieldBorder = OutlineInputBorder(
    borderRadius: BorderRadius.all(Radius.circular(8)),
    borderSide: BorderSide(color: ConnectoColors.borderStrong),
  );

  return ThemeData(
    brightness: Brightness.dark,
    colorScheme: scheme,
    fontFamily: fontFamily,
    useMaterial3: true,
    scaffoldBackgroundColor: ConnectoColors.bg,
    visualDensity: VisualDensity.standard,
    dividerTheme: const DividerThemeData(
      color: ConnectoColors.border,
      space: 1,
      thickness: 1,
    ),
    textTheme: const TextTheme(
      bodyLarge: TextStyle(fontSize: 14, height: 1.45),
      bodyMedium: TextStyle(fontSize: 14, height: 1.45),
      bodySmall: TextStyle(
        fontSize: 12,
        height: 1.35,
        color: ConnectoColors.muted,
      ),
      titleLarge: TextStyle(fontSize: 17, fontWeight: FontWeight.w600),
      titleMedium: TextStyle(fontSize: 15, fontWeight: FontWeight.w600),
      titleSmall: TextStyle(fontSize: 14, fontWeight: FontWeight.w600),
      labelLarge: label,
      labelMedium: label,
    ),
    filledButtonTheme: FilledButtonThemeData(style: button),
    outlinedButtonTheme: OutlinedButtonThemeData(
      style: button.copyWith(
        foregroundColor: WidgetStateProperty.resolveWith(
          (states) => states.contains(WidgetState.disabled)
              ? ConnectoColors.muted.withValues(alpha: 0.55)
              : ConnectoColors.text,
        ),
        side: WidgetStateProperty.resolveWith(
          (states) =>
              focusBorder.resolve(states) ??
              const BorderSide(color: ConnectoColors.borderStrong),
        ),
      ),
    ),
    textButtonTheme: TextButtonThemeData(style: button),
    iconButtonTheme: IconButtonThemeData(
      style: ButtonStyle(
        minimumSize: const WidgetStatePropertyAll(Size(36, 36)),
        iconSize: const WidgetStatePropertyAll(18),
        side: focusBorder,
      ),
    ),
    inputDecorationTheme: const InputDecorationTheme(
      filled: true,
      fillColor: ConnectoColors.bg,
      hintStyle: TextStyle(color: ConnectoColors.muted),
      contentPadding: EdgeInsets.symmetric(horizontal: 12, vertical: 10),
      border: fieldBorder,
      enabledBorder: fieldBorder,
      focusedBorder: OutlineInputBorder(
        borderRadius: BorderRadius.all(Radius.circular(8)),
        borderSide: BorderSide(color: ConnectoColors.accent, width: 2),
      ),
    ),
    dialogTheme: const DialogThemeData(
      backgroundColor: ConnectoColors.surface,
      surfaceTintColor: Colors.transparent,
      shape: RoundedRectangleBorder(
        side: BorderSide(color: ConnectoColors.border),
        borderRadius: BorderRadius.all(Radius.circular(12)),
      ),
    ),
    progressIndicatorTheme: const ProgressIndicatorThemeData(
      color: ConnectoColors.accent,
      linearMinHeight: 2,
    ),
  );
}
