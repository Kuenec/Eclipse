.class public Landroid/telephony/cdma/CdmaCellLocation;
.super Landroid/telephony/CellLocation;

.method public constructor <init>()V
    .registers 1

    invoke-direct {p0}, Landroid/telephony/CellLocation;-><init>()V

    return-void
.end method

.method public static convertQuartSecToDecDegrees(I)D
    .registers 5

    const v0, -0x278d00

    if-lt p0, v0, :invalid_coordinate

    const v0, 0x278d00

    if-gt p0, v0, :invalid_coordinate

    int-to-double v0, p0

    const-wide v2, 0x40cc200000000000L

    div-double/2addr v0, v2

    return-wide v0

    :invalid_coordinate
    new-instance v0, Ljava/lang/IllegalArgumentException;

    new-instance v1, Ljava/lang/StringBuilder;

    const-string v2, "Invalid coordiante value:"

    invoke-direct {v1, v2}, Ljava/lang/StringBuilder;-><init>(Ljava/lang/String;)V

    invoke-virtual {v1, p0}, Ljava/lang/StringBuilder;->append(I)Ljava/lang/StringBuilder;

    move-result-object v1

    invoke-virtual {v1}, Ljava/lang/StringBuilder;->toString()Ljava/lang/String;

    move-result-object v1

    invoke-direct {v0, v1}, Ljava/lang/IllegalArgumentException;-><init>(Ljava/lang/String;)V

    throw v0
.end method
