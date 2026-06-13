# ECLIPSE PATCH 2026-06-13: AOSP android.view.Display.Mode (nested). ATL's installed Display omits both
# getMode() and this Mode class; Roblox queries Display.getMode() in ActivityNativeMain.onResume startup
# (NoSuchMethodError without it). Display.getMode() (added in patch-framework.sh) constructs one of these
# from window_width/window_height + 60.0f (consistent with the installed Display.getWidth/getHeight/
# getRefreshRate). Public static final nested class (accessFlags 0x19), like AOSP.
.class public final Landroid/view/Display$Mode;
.super Ljava/lang/Object;


# annotations
.annotation system Ldalvik/annotation/EnclosingClass;
    value = Landroid/view/Display;
.end annotation

.annotation system Ldalvik/annotation/InnerClass;
    accessFlags = 0x19
    name = "Mode"
.end annotation


# instance fields
.field private final mModeId:I

.field private final mWidth:I

.field private final mHeight:I

.field private final mRefreshRate:F


# direct methods
.method public constructor <init>(IIIF)V
    .registers 5

    invoke-direct {p0}, Ljava/lang/Object;-><init>()V

    iput p1, p0, Landroid/view/Display$Mode;->mModeId:I

    iput p2, p0, Landroid/view/Display$Mode;->mWidth:I

    iput p3, p0, Landroid/view/Display$Mode;->mHeight:I

    iput p4, p0, Landroid/view/Display$Mode;->mRefreshRate:F

    return-void
.end method


# virtual methods
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
