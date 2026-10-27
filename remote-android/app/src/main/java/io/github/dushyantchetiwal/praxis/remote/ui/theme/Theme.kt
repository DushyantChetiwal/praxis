package io.github.dushyantchetiwal.praxis.remote.ui.theme

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Shapes
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/** The green of the "online" dot, from the app icon. */
val OnlineGreen = Color(0xFF34D399)

// Calm neutrals with a single indigo accent, the same palette as the Praxis
// desktop theme, so the phone and the computer look like one product. The
// wallpaper-derived dynamic colors are deliberately not used for that reason.
private val LightColors = lightColorScheme(
    primary = Color(0xFF4F46E5),
    onPrimary = Color.White,
    primaryContainer = Color(0xFFE8E7FD),
    onPrimaryContainer = Color(0xFF1E1B4B),
    secondary = Color(0xFF52525B),
    onSecondary = Color.White,
    secondaryContainer = Color(0xFFECECEF),
    onSecondaryContainer = Color(0xFF18181B),
    tertiary = Color(0xFF0E7C5A),
    onTertiary = Color.White,
    tertiaryContainer = Color(0xFFD5F5E8),
    onTertiaryContainer = Color(0xFF002116),
    error = Color(0xFFDC2626),
    onError = Color.White,
    errorContainer = Color(0xFFFDECEC),
    onErrorContainer = Color(0xFF7F1D1D),
    background = Color(0xFFFAFAFA),
    onBackground = Color(0xFF18181B),
    surface = Color(0xFFFAFAFA),
    onSurface = Color(0xFF18181B),
    surfaceVariant = Color(0xFFF0F0F2),
    onSurfaceVariant = Color(0xFF5B5B66),
    surfaceContainerLowest = Color(0xFFFFFFFF),
    surfaceContainerLow = Color(0xFFF5F5F6),
    surfaceContainer = Color(0xFFF0F0F2),
    surfaceContainerHigh = Color(0xFFEAEAED),
    surfaceContainerHighest = Color(0xFFE4E4E8),
    outline = Color(0xFF8E8E99),
    outlineVariant = Color(0xFFE4E4E7),
)

private val DarkColors = darkColorScheme(
    primary = Color(0xFFA5A8FF),
    onPrimary = Color(0xFF1E1B4B),
    primaryContainer = Color(0xFF2B2A5E),
    onPrimaryContainer = Color(0xFFE0E0FF),
    secondary = Color(0xFFA1A1AA),
    onSecondary = Color(0xFF18181B),
    secondaryContainer = Color(0xFF26262C),
    onSecondaryContainer = Color(0xFFECECEF),
    tertiary = Color(0xFF8AD5B6),
    onTertiary = Color(0xFF00382A),
    tertiaryContainer = Color(0xFF0F3A2C),
    onTertiaryContainer = Color(0xFFA6F2D2),
    error = Color(0xFFF87171),
    onError = Color(0xFF450A0A),
    errorContainer = Color(0xFF3A1416),
    onErrorContainer = Color(0xFFFECACA),
    background = Color(0xFF0F0F12),
    onBackground = Color(0xFFECECEF),
    surface = Color(0xFF0F0F12),
    onSurface = Color(0xFFECECEF),
    surfaceVariant = Color(0xFF1A1A1F),
    onSurfaceVariant = Color(0xFFA1A1AA),
    surfaceContainerLowest = Color(0xFF0B0B0D),
    surfaceContainerLow = Color(0xFF151519),
    surfaceContainer = Color(0xFF1A1A1F),
    surfaceContainerHigh = Color(0xFF212127),
    surfaceContainerHighest = Color(0xFF28282F),
    outline = Color(0xFF71717A),
    outlineVariant = Color(0xFF2A2A31),
)

private val BaseType = Typography()

private val PraxisTypography = Typography(
    headlineSmall = BaseType.headlineSmall.copy(fontWeight = FontWeight.SemiBold, letterSpacing = (-0.2).sp),
    titleLarge = BaseType.titleLarge.copy(fontWeight = FontWeight.SemiBold, letterSpacing = (-0.2).sp),
    titleMedium = BaseType.titleMedium.copy(fontWeight = FontWeight.SemiBold),
    titleSmall = BaseType.titleSmall.copy(fontWeight = FontWeight.SemiBold),
    bodyLarge = BaseType.bodyLarge.copy(lineHeight = 24.sp),
    bodyMedium = BaseType.bodyMedium.copy(fontSize = 15.sp, lineHeight = 22.sp, letterSpacing = 0.1.sp),
    bodySmall = BaseType.bodySmall.copy(fontSize = 13.sp, lineHeight = 18.sp, letterSpacing = 0.1.sp),
    labelLarge = BaseType.labelLarge.copy(fontWeight = FontWeight.Medium),
    labelMedium = BaseType.labelMedium.copy(fontWeight = FontWeight.Medium, letterSpacing = 0.2.sp),
    labelSmall = BaseType.labelSmall.copy(letterSpacing = 0.2.sp),
)

private val PraxisShapes = Shapes(
    extraSmall = RoundedCornerShape(6.dp),
    small = RoundedCornerShape(10.dp),
    medium = RoundedCornerShape(14.dp),
    large = RoundedCornerShape(20.dp),
    extraLarge = RoundedCornerShape(28.dp),
)

@Composable
fun PraxisTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = if (isSystemInDarkTheme()) DarkColors else LightColors,
        typography = PraxisTypography,
        shapes = PraxisShapes,
        content = content,
    )
}
