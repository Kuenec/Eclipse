




.class public final Landroid/view/Display$Mode;
.super Ljava/lang/Object;



.annotation system Ldalvik/annotation/EnclosingClass;
    value = Landroid/view/Display;
.end annotation

.annotation system Ldalvik/annotation/InnerClass;
    accessFlags = 0x19
    name = "Mode"
.end annotation



.field private final mModeId:I

.field private final mWidth:I

.field private final mHeight:I

.field private final mRefreshRate:F



.method public constructor <init>(IIIF)V
    .registers 5

    invoke-direct {p0}, Ljava/lang/Object;-><init>()V

    iput p1, p0, Landroid/view/Display$Mode;->mModeId:I

    iput p2, p0, Landroid/view/Display$Mode;->mWidth:I

    iput p3, p0, Landroid/view/Display$Mode;->mHeight:I

    iput p4, p0, Landroid/view/Display$Mode;->mRefreshRate:F

    return-void
.end method



.method public getModeId()I
    .registers 2

    iget v0, p0, Landroid/view/Display$Mode;->mModeId:I

    return v0
.end method

.method public getPhysicalWidth()I
    .registers 2

    iget v0, p0, Landroid/view/Display$Mode;->mWidth:I

    return v0
.end method

.method public getPhysicalHeight()I
    .registers 2

    iget v0, p0, Landroid/view/Display$Mode;->mHeight:I

    return v0
.end method

.method public getRefreshRate()F
    .registers 2

    iget v0, p0, Landroid/view/Display$Mode;->mRefreshRate:F

    return v0
.end method
