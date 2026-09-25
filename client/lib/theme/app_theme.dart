import 'package:flutter/material.dart';

/// Adjutant design tokens — the compiled vocabulary of the design language.
///
/// Every visual value used by a widget comes from here. Plugins never supply
/// colours, type, or spacing; they supply data, and the client renders it with
/// these tokens (see `docs/design/client-and-plugin-ui.md` §2).
///
/// Source of truth: `docs/design/flutter-design-language.md`.
class AppColors {
  const AppColors._();

  // Primary — Vermont forests. Go, active, approved.
  static const primary = Color(0xFF2E7D32);
  static const onPrimary = Color(0xFFFFFFFF);
  static const primaryContainer = Color(0xFFA5D6A7);
  static const onPrimaryContainer = Color(0xFF1B5E20);
  static const primaryDark = Color(0xFF256828);

  // Secondary — earth, wood, the trail.
  static const secondary = Color(0xFF5D4037);
  static const onSecondary = Color(0xFFFFFFFF);
  static const secondaryContainer = Color(0xFFD7CCC8);
  static const onSecondaryContainer = Color(0xFF3E2723);

  // Tertiary — sky, water, information.
  static const tertiary = Color(0xFF1565C0);
  static const onTertiary = Color(0xFFFFFFFF);
  static const tertiaryContainer = Color(0xFFBBDEFB);
  static const onTertiaryContainer = Color(0xFF0D47A1);

  // Semantic.
  static const success = Color(0xFF2E7D32);
  static const warning = Color(0xFFF57F17);
  static const error = Color(0xFFC62828);
  static const info = Color(0xFF1565C0);

  // Surfaces — light.
  static const surface = Color(0xFFFAFAFA);
  static const surfaceDim = Color(0xFFF5F5F5);
  static const surfaceContainer = Color(0xFFEEEEEE);
  static const outline = Color(0xFFBDBDBD);
  static const outlineVariant = Color(0xFFE0E0E0);
  static const onSurface = Color(0xFF212121);
  static const onSurfaceMuted = Color(0xFF757575);

  // Surfaces — dark. Night hunts, evening meetings.
  static const surfaceDark = Color(0xFF121212);
  static const surfaceDimDark = Color(0xFF1E1E1E);
  static const surfaceContainerDark = Color(0xFF2C2C2C);
  static const outlineDark = Color(0xFF424242);
  static const outlineVariantDark = Color(0xFF333333);
  static const onSurfaceDark = Color(0xFFE0E0E0);
  static const onSurfaceMutedDark = Color(0xFF9E9E9E);

  /// Container colour for a status, used by badges and status chips.
  static Color statusContainer(String status, Brightness brightness) {
    final dark = brightness == Brightness.dark;
    switch (status.toLowerCase()) {
      case 'approved':
      case 'active':
      case 'completed':
      case 'passed':
        return dark ? const Color(0xFF1B5E20) : primaryContainer;
      case 'pending':
      case 'review':
      case 'voting':
        return dark ? const Color(0xFF3E2723) : const Color(0xFFFFF3E0);
      case 'rejected':
      case 'failed':
      case 'cancelled':
        return dark ? const Color(0xFF3E1A1A) : const Color(0xFFFFEBEE);
      case 'in_progress':
      case 'execution':
        return dark ? const Color(0xFF0D47A1) : tertiaryContainer;
      default:
        return dark ? surfaceContainerDark : surfaceContainer;
    }
  }

  /// Foreground colour for a status, legible on [statusContainer].
  static Color statusForeground(String status, Brightness brightness) {
    final dark = brightness == Brightness.dark;
    switch (status.toLowerCase()) {
      case 'approved':
      case 'active':
      case 'completed':
      case 'passed':
        return dark ? primaryContainer : onPrimaryContainer;
      case 'pending':
      case 'review':
      case 'voting':
        return dark ? const Color(0xFFFFB74D) : const Color(0xFFE65100);
      case 'rejected':
      case 'failed':
      case 'cancelled':
        return dark ? const Color(0xFFEF9A9A) : error;
      case 'in_progress':
      case 'execution':
        return dark ? tertiaryContainer : onTertiaryContainer;
      default:
        return dark ? onSurfaceMutedDark : onSurfaceMuted;
    }
  }
}

/// Spacing scale. No widget invents a spacing value.
class AppSpacing {
  const AppSpacing._();

  static const xs = 4.0;
  static const sm = 8.0;
  static const md = 16.0;
  static const lg = 24.0;
  static const xl = 32.0;
  static const xxl = 48.0;

  /// Outdoor rule: 48dp minimum, 56dp for primary actions.
  static const touchTargetMin = 48.0;
  static const touchTargetPrimary = 56.0;
}

class AppRadius {
  const AppRadius._();

  static const sm = 8.0;
  static const md = 12.0;
  static const lg = 16.0;
}

/// Type scale. Minimum body size is 14sp — outdoor readability.
class AppText {
  const AppText._();

  static const displayLarge = TextStyle(fontSize: 32, fontWeight: FontWeight.w700, height: 1.2);
  static const displayMedium = TextStyle(fontSize: 28, fontWeight: FontWeight.w700, height: 1.2);
  static const headlineLarge = TextStyle(fontSize: 24, fontWeight: FontWeight.w600, height: 1.2);
  static const headlineMedium = TextStyle(fontSize: 20, fontWeight: FontWeight.w600, height: 1.2);
  static const titleLarge = TextStyle(fontSize: 18, fontWeight: FontWeight.w500, height: 1.3);
  static const titleMedium = TextStyle(fontSize: 16, fontWeight: FontWeight.w500, height: 1.3);
  static const bodyLarge = TextStyle(fontSize: 16, fontWeight: FontWeight.w400, height: 1.5);
  static const bodyMedium = TextStyle(fontSize: 14, fontWeight: FontWeight.w400, height: 1.5);
  static const bodySmall = TextStyle(fontSize: 12, fontWeight: FontWeight.w400, height: 1.4);
  static const labelLarge = TextStyle(fontSize: 14, fontWeight: FontWeight.w500, height: 1.2);
  static const labelMedium = TextStyle(fontSize: 12, fontWeight: FontWeight.w500, height: 1.2);
  static const labelSmall = TextStyle(fontSize: 10, fontWeight: FontWeight.w500, height: 1.2);
}

/// The app theme, light and dark. Both are first-class: dark mode is not an
/// afterthought — it is what a scout uses at a night hunt or an evening meeting.
class AppTheme {
  const AppTheme._();

  static ThemeData light() => _build(Brightness.light);
  static ThemeData dark() => _build(Brightness.dark);

  static ThemeData _build(Brightness brightness) {
    final isDark = brightness == Brightness.dark;

    final scheme = ColorScheme(
      brightness: brightness,
      primary: AppColors.primary,
      onPrimary: AppColors.onPrimary,
      primaryContainer: isDark ? AppColors.onPrimaryContainer : AppColors.primaryContainer,
      onPrimaryContainer: isDark ? AppColors.primaryContainer : AppColors.onPrimaryContainer,
      secondary: AppColors.secondary,
      onSecondary: AppColors.onSecondary,
      secondaryContainer: isDark ? AppColors.onSecondaryContainer : AppColors.secondaryContainer,
      onSecondaryContainer: isDark ? AppColors.secondaryContainer : AppColors.onSecondaryContainer,
      tertiary: AppColors.tertiary,
      onTertiary: AppColors.onTertiary,
      tertiaryContainer: isDark ? AppColors.onTertiaryContainer : AppColors.tertiaryContainer,
      onTertiaryContainer: isDark ? AppColors.tertiaryContainer : AppColors.onTertiaryContainer,
      error: AppColors.error,
      onError: Colors.white,
      surface: isDark ? AppColors.surfaceDark : AppColors.surface,
      onSurface: isDark ? AppColors.onSurfaceDark : AppColors.onSurface,
      surfaceContainerHighest: isDark ? AppColors.surfaceContainerDark : AppColors.surfaceContainer,
      outline: isDark ? AppColors.outlineDark : AppColors.outline,
      outlineVariant: isDark ? AppColors.outlineVariantDark : AppColors.outlineVariant,
    );

    final base = isDark ? ThemeData.dark() : ThemeData.light();

    return base.copyWith(
      colorScheme: scheme,
      scaffoldBackgroundColor: scheme.surface,
      textTheme: base.textTheme.apply(
        bodyColor: scheme.onSurface,
        displayColor: scheme.onSurface,
      ),
      appBarTheme: AppBarTheme(
        backgroundColor: scheme.surface,
        foregroundColor: scheme.onSurface,
        elevation: 0,
        scrolledUnderElevation: 0,
        centerTitle: false,
        titleTextStyle: AppText.headlineMedium.copyWith(color: scheme.onSurface),
      ),
      cardTheme: CardThemeData(
        color: scheme.surface,
        elevation: 0,
        margin: EdgeInsets.zero,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(AppRadius.md),
          side: BorderSide(color: scheme.outlineVariant),
        ),
      ),
      filledButtonTheme: FilledButtonThemeData(
        style: FilledButton.styleFrom(
          backgroundColor: scheme.primary,
          foregroundColor: scheme.onPrimary,
          minimumSize: const Size(0, AppSpacing.touchTargetMin),
          padding: const EdgeInsets.symmetric(horizontal: AppSpacing.lg),
          shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(AppRadius.sm)),
          textStyle: AppText.labelLarge,
        ),
      ),
      outlinedButtonTheme: OutlinedButtonThemeData(
        style: OutlinedButton.styleFrom(
          foregroundColor: scheme.primary,
          minimumSize: const Size(0, AppSpacing.touchTargetMin),
          padding: const EdgeInsets.symmetric(horizontal: AppSpacing.lg),
          side: BorderSide(color: scheme.primary),
          shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(AppRadius.sm)),
          textStyle: AppText.labelLarge,
        ),
      ),
      textButtonTheme: TextButtonThemeData(
        style: TextButton.styleFrom(
          foregroundColor: scheme.primary,
          minimumSize: const Size(0, AppSpacing.touchTargetMin),
          textStyle: AppText.labelLarge,
        ),
      ),
      inputDecorationTheme: InputDecorationTheme(
        filled: true,
        fillColor: scheme.surface,
        contentPadding: const EdgeInsets.symmetric(
          horizontal: AppSpacing.md,
          vertical: AppSpacing.md,
        ),
        border: OutlineInputBorder(
          borderRadius: BorderRadius.circular(AppRadius.sm),
          borderSide: BorderSide(color: scheme.outline),
        ),
        enabledBorder: OutlineInputBorder(
          borderRadius: BorderRadius.circular(AppRadius.sm),
          borderSide: BorderSide(color: scheme.outline),
        ),
        focusedBorder: OutlineInputBorder(
          borderRadius: BorderRadius.circular(AppRadius.sm),
          borderSide: BorderSide(color: scheme.primary, width: 2),
        ),
        labelStyle: AppText.bodyMedium.copyWith(color: scheme.onSurface),
        hintStyle: AppText.bodyMedium.copyWith(color: scheme.outline),
      ),
      navigationBarTheme: NavigationBarThemeData(
        backgroundColor: scheme.surface,
        indicatorColor: scheme.primaryContainer,
        height: 64,
        labelTextStyle: WidgetStateProperty.all(AppText.labelMedium),
      ),
      navigationRailTheme: NavigationRailThemeData(
        backgroundColor: isDark ? AppColors.surfaceDimDark : AppColors.surfaceDim,
        indicatorColor: scheme.primaryContainer,
        selectedLabelTextStyle: AppText.labelMedium,
        unselectedLabelTextStyle: AppText.labelMedium,
      ),
      dividerTheme: DividerThemeData(color: scheme.outlineVariant, thickness: 1, space: 1),
      chipTheme: ChipThemeData(
        labelStyle: AppText.labelMedium,
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(20)),
        side: BorderSide(color: scheme.outlineVariant),
      ),
    );
  }
}
