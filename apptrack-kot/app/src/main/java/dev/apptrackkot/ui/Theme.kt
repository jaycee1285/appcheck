package dev.apptrackkot.ui

import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.ColorScheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import dev.apptrackkot.AppTrackKotPalette

@Composable
fun AppTrackKotTheme(
    dynamicColor: Boolean = false,
    content: @Composable () -> Unit,
) {
    val colorScheme = darkColorScheme(
        primary = AppTrackKotPalette.Accent,
        background = AppTrackKotPalette.Background,
        surface = Color(0xFF1B1C1E),
        onPrimary = Color.White,
        onBackground = AppTrackKotPalette.Foreground,
        onSurface = AppTrackKotPalette.Foreground,
        secondaryContainer = Color(0xFF2A2C30),
        onSecondaryContainer = AppTrackKotPalette.Foreground,
    )
    MaterialTheme(
        colorScheme = colorScheme,
        shapes = MaterialTheme.shapes.copy(medium = RoundedCornerShape(8.dp)),
        content = content,
    )
}