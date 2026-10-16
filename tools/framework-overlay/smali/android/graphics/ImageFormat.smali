.class public Landroid/graphics/ImageFormat;
.super Ljava/lang/Object;

.method public constructor <init>()V
    .registers 1

    invoke-direct {p0}, Ljava/lang/Object;-><init>()V

    return-void
.end method

.method public static getBitsPerPixel(I)I
    .registers 2

    sparse-switch p0, :bits_per_pixel_by_format

    const/4 v0, -0x1

    return v0

    :eight_bits
    const/16 v0, 0x8

    return v0

    :ten_bits
    const/16 v0, 0xa

    return v0

    :twelve_bits
    const/16 v0, 0xc

    return v0

    :sixteen_bits
    const/16 v0, 0x10

    return v0

    :twenty_four_bits
    const/16 v0, 0x18

    return v0

    :thirty_two_bits
    const/16 v0, 0x20

    return v0

    :bits_per_pixel_by_format
    .sparse-switch
        0x4 -> :sixteen_bits
        0x10 -> :sixteen_bits
        0x11 -> :twelve_bits
        0x14 -> :sixteen_bits
        0x20 -> :sixteen_bits
        0x23 -> :twelve_bits
        0x25 -> :ten_bits
        0x26 -> :twelve_bits
        0x27 -> :sixteen_bits
        0x28 -> :twenty_four_bits
        0x29 -> :twenty_four_bits
        0x2a -> :thirty_two_bits
        0x1002 -> :sixteen_bits
        0x20203859 -> :eight_bits
        0x20363159 -> :sixteen_bits
        0x32315659 -> :twelve_bits
        0x44363159 -> :sixteen_bits
    .end sparse-switch
.end method
